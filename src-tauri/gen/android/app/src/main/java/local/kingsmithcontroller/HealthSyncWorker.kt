package local.kingsmithcontroller

import android.content.Context
import androidx.health.connect.client.HealthConnectClient
import androidx.health.connect.client.permission.HealthPermission
import androidx.health.connect.client.records.DistanceRecord
import androidx.health.connect.client.records.ExerciseSessionRecord
import androidx.health.connect.client.records.HeartRateRecord
import androidx.health.connect.client.records.StepsRecord
import androidx.work.BackoffPolicy
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.util.concurrent.TimeUnit

internal object HealthSync {
    const val WORK = "kingsmith-health-connect"
    val permissions = setOf(
        HealthPermission.getWritePermission(ExerciseSessionRecord::class),
        HealthPermission.getWritePermission(DistanceRecord::class),
        HealthPermission.getWritePermission(StepsRecord::class),
        HealthPermission.getWritePermission(HeartRateRecord::class),
    )
    fun preferences(context: Context) = context.getSharedPreferences("health-connect", Context.MODE_PRIVATE)
    fun enabled(context: Context) = preferences(context).getBoolean("enabled", false)
    fun schedule(context: Context, restart: Boolean = false) {
        if (!enabled(context) || preferences(context).getString("directory", null) == null) return
        // Append a catch-up scan even if a scan is finishing right now. KEEP could lose that wakeup.
        val policy = if (restart) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.APPEND_OR_REPLACE
        WorkManager.getInstance(context).enqueueUniqueWork(WORK, policy,
            OneTimeWorkRequestBuilder<HealthSyncWorker>()
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS).build())
    }
}

class HealthSyncWorker(context: Context, parameters: WorkerParameters) : CoroutineWorker(context, parameters) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val context = applicationContext
        val preferences = HealthSync.preferences(context)
        if (!HealthSync.enabled(context)) return@withContext Result.success()
        try {
            if (HealthConnectClient.getSdkStatus(context) != HealthConnectClient.SDK_AVAILABLE) {
                preferences.edit().putString("message", "Health Connect needs to be installed or updated. Pending walks will retry.").apply()
                return@withContext Result.retry()
            }
            val client = HealthConnectClient.getOrCreate(context)
            if (!client.permissionController.getGrantedPermissions().containsAll(HealthSync.permissions)) {
                preferences.edit().putString("message", "Sync paused. Allow all four Health Connect write permissions to continue.").apply()
                return@withContext Result.success()
            }
            val path = preferences.getString("directory", null) ?: return@withContext Result.success()
            val report = syncHealthWalks(File(path), { HealthSync.enabled(context) }) { records ->
                // Permission can be revoked while a long catch-up is running.
                if (!client.permissionController.getGrantedPermissions().containsAll(HealthSync.permissions)) throw SecurityException()
                client.insertRecords(records)
            }
            val update = preferences.edit().putInt("syncedWalks", report.total).putString("message",
                if (report.skipped > 0) "${report.skipped} saved walks have invalid or empty data and could not be synced." else null)
            if (report.uploaded > 0) update.putLong("lastSyncedMs", System.currentTimeMillis())
            update.apply()
            Result.success()
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: SecurityException) {
            preferences.edit().putString("message", "Sync paused. Restore Health Connect permissions to continue.").apply()
            Result.success()
        } catch (_: Exception) {
            // No health payloads, timestamps, or BPM values go into diagnostics.
            preferences.edit().putString("message", "Couldn't sync saved walks. Android will retry automatically.").apply()
            Result.retry()
        }
    }
}
