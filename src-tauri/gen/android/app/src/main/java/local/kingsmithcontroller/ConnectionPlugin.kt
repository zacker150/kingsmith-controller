package local.kingsmithcontroller

import android.app.Activity
import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
class ConnectionArgs {
    var active: Boolean = false
}

/** Called from Rust connection state, never from WebView visibility events. */
@TauriPlugin
class ConnectionPlugin(private val activity: Activity) : Plugin(activity) {
    @Command
    fun setActive(invoke: Invoke) {
        val active = invoke.parseArgs(ConnectionArgs::class.java).active
        activity.runOnUiThread {
            try {
                val service = Intent(activity, ConnectionService::class.java)
                if (active) {
                    if (activity.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) {
                        invoke.reject("Nearby devices permission is required to keep Bluetooth running")
                        return@runOnUiThread
                    }
                    activity.startForegroundService(service)
                    // Notification permission is optional for the service itself.
                    // Ask once, after the user has initiated a connection.
                    val preferences = activity.getSharedPreferences("connections", 0)
                    if (Build.VERSION.SDK_INT >= 33
                        && activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
                        && !preferences.getBoolean("notification_requested", false)) {
                        preferences.edit().putBoolean("notification_requested", true).apply()
                        activity.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1002)
                    }
                } else activity.stopService(service)
                invoke.resolve()
            } catch (error: Exception) {
                invoke.reject("Couldn't keep Bluetooth running in the background: ${error.message}")
            }
        }
    }
}
