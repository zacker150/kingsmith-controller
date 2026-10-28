package local.kingsmithcontroller

import android.app.Activity
import android.content.ContentValues
import android.os.Environment
import android.provider.MediaStore
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import kotlin.concurrent.thread

@InvokeArg
class DownloadArgs {
    lateinit var filename: String
    lateinit var content: String
}

/** App-owned MediaStore downloads need no storage permission on Android 10+. */
@TauriPlugin
class DownloadsPlugin(private val activity: Activity) : Plugin(activity) {
    @Command
    fun saveTcx(invoke: Invoke) {
        val args = invoke.parseArgs(DownloadArgs::class.java)
        if (!Regex("kingsmith-walk-[0-9]+-[0-9]+\\.tcx").matches(args.filename)) {
            invoke.reject("Invalid walk filename")
            return
        }
        thread(name = "walk-tcx-download") {
            val resolver = activity.contentResolver
            var uri: android.net.Uri? = null
            try {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, args.filename)
                    put(MediaStore.Downloads.MIME_TYPE, "application/vnd.garmin.tcx+xml")
                    put(MediaStore.Downloads.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/Kingsmith Controller")
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                val target = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: error("Couldn't create download")
                uri = target
                (resolver.openOutputStream(target) ?: error("Couldn't open download")).use {
                    it.write(args.content.toByteArray(Charsets.UTF_8))
                }
                val published = resolver.update(target, ContentValues().apply {
                    put(MediaStore.Downloads.IS_PENDING, 0)
                }, null, null)
                check(published == 1) { "Couldn't publish download" }
                val name = resolver.query(target, arrayOf(MediaStore.Downloads.DISPLAY_NAME), null, null, null)?.use {
                    if (it.moveToFirst()) it.getString(0) else args.filename
                } ?: args.filename
                invoke.resolve(JSObject().put("location", "Downloads/Kingsmith Controller/$name"))
            } catch (error: Exception) {
                uri?.let { runCatching { resolver.delete(it, null, null) } }
                invoke.reject("Couldn't save TCX: ${error.message}")
            }
        }
    }
}
