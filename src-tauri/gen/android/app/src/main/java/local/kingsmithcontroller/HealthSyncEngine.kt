package local.kingsmithcontroller

import androidx.health.connect.client.records.Record
import kotlinx.coroutines.ensureActive
import org.json.JSONException
import java.io.File
import java.io.FileNotFoundException
import java.io.FileOutputStream
import java.io.IOException
import kotlin.coroutines.coroutineContext

internal data class HealthSyncReport(val uploaded: Int, val skipped: Int, val total: Int)

/** Saved JSON is the durable outbox; receipts are written only after every record succeeds. */
internal suspend fun syncHealthWalks(
    directory: File,
    enabled: () -> Boolean,
    insert: suspend (List<Record>) -> Unit,
): HealthSyncReport {
    if (!directory.exists()) return HealthSyncReport(0, 0, 0)
    val receipts = File(directory, "health-connect")
    if (!receipts.isDirectory && !receipts.mkdirs()) throw IOException("Couldn't save Health Connect receipts")
    var uploaded = 0
    var skipped = 0
    // Only enumerate filenames; never load active checkpoints or already-synced walk contents.
    val files = directory.listFiles() ?: throw IOException("Couldn't read saved walks")
    walkLoop@ for (file in files) {
        coroutineContext.ensureActive()
        if (!enabled()) break
        val id = HealthWalk.FILE.matchEntire(file.name)?.groupValues?.get(1) ?: continue
        val receipt = File(receipts, "$id.synced")
        if (receipt.isFile) continue
        val records = try {
            HealthWalk.parse(id, file.readText()).records()
        } catch (_: FileNotFoundException) {
            // A walk deleted locally before export is no longer pending.
            if (file.exists()) throw IOException("Couldn't read saved walk")
            continue
        } catch (_: IllegalArgumentException) {
            skipped++
            continue
        } catch (_: JSONException) {
            skipped++
            continue
        }
        for (batch in records.chunked(100)) {
            coroutineContext.ensureActive()
            if (!enabled() || !file.isFile) return HealthSyncReport(uploaded, skipped, receiptCount(receipts))
            try { insert(batch) }
            catch (_: IllegalArgumentException) {
                // A provider-rejected record must not block every later saved walk.
                skipped++
                continue@walkLoop
            }
        }
        // A crash before this point retries with the SAME client IDs and version.
        // A receipt also respects records the user later deletes in Health Connect.
        FileOutputStream(receipt).use { it.write(byteArrayOf(1)); it.fd.sync() }
        uploaded++
    }
    return HealthSyncReport(uploaded, skipped, receiptCount(receipts))
}

private fun receiptCount(directory: File): Int = directory.listFiles()?.count {
    it.isFile && it.name.endsWith(".synced")
} ?: 0
