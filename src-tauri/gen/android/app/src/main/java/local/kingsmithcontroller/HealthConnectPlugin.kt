package local.kingsmithcontroller

import android.app.Activity
import android.content.Intent
import android.net.Uri
import androidx.activity.result.ActivityResult
import androidx.appcompat.app.AppCompatActivity
import androidx.health.connect.client.HealthConnectClient
import androidx.health.connect.client.PermissionController
import androidx.work.WorkManager
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File

@InvokeArg
class HealthDirectoryArgs { lateinit var directory: String }

@TauriPlugin
class HealthConnectPlugin(private val activity: Activity) : Plugin(activity) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var requesting = false

    private fun run(invoke: Invoke, block: suspend () -> Unit) {
        scope.launch {
            try { block() }
            catch (cancelled: CancellationException) { throw cancelled }
            catch (_: Exception) {
                HealthSync.preferences(activity).edit().putString("message", "Couldn't access Health Connect. Pending walks will retry.").apply()
                invoke.reject("Couldn't access Health Connect. Please try again.")
            }
        }
    }

    @Command
    fun configure(invoke: Invoke) = run(invoke) {
        val directory = File(invoke.parseArgs(HealthDirectoryArgs::class.java).directory).canonicalFile
        // Only the real app's saved walks can be exported; never mock history or arbitrary paths.
        require(directory.name == "walks" && directory.parentFile == activity.dataDir.canonicalFile)
        check(HealthSync.preferences(activity).edit().putString("directory", directory.path).commit())
        HealthSync.schedule(activity)
        invoke.resolve()
    }

    @Command
    fun sync(invoke: Invoke) = run(invoke) {
        HealthSync.schedule(activity)
        invoke.resolve()
    }

    @Command
    fun status(invoke: Invoke) = run(invoke) { invoke.resolve(snapshot()) }

    private suspend fun snapshot(): JSObject {
        val sdk = HealthConnectClient.getSdkStatus(activity)
        val granted = sdk == HealthConnectClient.SDK_AVAILABLE &&
            HealthConnectClient.getOrCreate(activity).permissionController.getGrantedPermissions().containsAll(HealthSync.permissions)
        val preferences = HealthSync.preferences(activity)
        return JSObject().apply {
            put("availability", when (sdk) {
                HealthConnectClient.SDK_AVAILABLE -> "available"
                HealthConnectClient.SDK_UNAVAILABLE_PROVIDER_UPDATE_REQUIRED -> "installRequired"
                else -> "unavailable"
            })
            put("enabled", HealthSync.enabled(activity))
            put("permitted", granted)
            put("syncedWalks", preferences.getInt("syncedWalks", 0))
            put("lastSyncedMs", preferences.getLong("lastSyncedMs", 0).takeIf { it > 0 } ?: JSONObject.NULL)
            put("message", preferences.getString("message", null) ?: JSONObject.NULL)
        }
    }

    @Command
    fun connect(invoke: Invoke) = run(invoke) {
        val sdk = HealthConnectClient.getSdkStatus(activity)
        if (sdk == HealthConnectClient.SDK_UNAVAILABLE_PROVIDER_UPDATE_REQUIRED) {
            withContext(Dispatchers.Main) { activity.startActivity(Intent(Intent.ACTION_VIEW,
                Uri.parse("market://details?id=com.google.android.apps.healthdata"))) }
            invoke.resolve(snapshot())
            return@run
        }
        check(sdk == HealthConnectClient.SDK_AVAILABLE)
        val client = HealthConnectClient.getOrCreate(activity)
        if (client.permissionController.getGrantedPermissions().containsAll(HealthSync.permissions)) {
            enable()
            invoke.resolve(snapshot())
        } else withContext(Dispatchers.Main) {
            if (requesting) { invoke.reject("A Health Connect permission request is already open"); return@withContext }
            requesting = true
            try {
                val intent = PermissionController.createRequestPermissionResultContract().createIntent(activity, HealthSync.permissions)
                startActivityForResult(invoke, intent, "permissionsResult")
            } catch (error: Exception) { requesting = false; throw error }
        }
    }

    @ActivityCallback
    fun permissionsResult(invoke: Invoke, result: ActivityResult) {
        requesting = false
        run(invoke) {
            // Recheck the system permission state, including a dismissed or partially granted request.
            val granted = HealthConnectClient.getOrCreate(activity).permissionController.getGrantedPermissions()
            if (granted.containsAll(HealthSync.permissions)) enable()
            else HealthSync.preferences(activity).edit().putString("message",
                "Allow workouts, distance, steps, and heart rate to enable automatic sync.").apply()
            invoke.resolve(snapshot())
        }
    }

    private fun enable() {
        check(HealthSync.preferences(activity).edit().putBoolean("enabled", true).remove("message").commit())
        // A deliberate reconnect should not wait behind an old provider-error backoff.
        HealthSync.schedule(activity, restart = true)
    }

    @Command
    fun disconnect(invoke: Invoke) = run(invoke) {
        check(HealthSync.preferences(activity).edit().putBoolean("enabled", false).remove("message").commit())
        WorkManager.getInstance(activity).cancelUniqueWork(HealthSync.WORK)
        invoke.resolve(snapshot())
    }

    @Command
    fun settings(invoke: Invoke) = run(invoke) {
        withContext(Dispatchers.Main) { activity.startActivity(Intent(HealthConnectClient.ACTION_HEALTH_CONNECT_SETTINGS)) }
        invoke.resolve()
    }

    override fun onResume(activity: AppCompatActivity) { HealthSync.schedule(activity) }
    override fun onDestroy(activity: AppCompatActivity) { scope.cancel() }
}
