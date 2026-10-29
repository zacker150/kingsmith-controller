package local.kingsmithcontroller

import androidx.health.connect.client.records.DistanceRecord
import androidx.health.connect.client.records.ExerciseSessionRecord
import androidx.health.connect.client.records.HeartRateRecord
import androidx.health.connect.client.records.Record
import androidx.health.connect.client.records.StepsRecord
import androidx.health.connect.client.records.metadata.Metadata
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.IOException
import java.time.Instant

class HealthSyncTest {
    @get:Rule val temporary = TemporaryFolder()

    private fun walk(id: String = "1000-1") = HealthWalk(id, 1000, 10_000, 9, 12, 20, false,
        listOf(HealthSample(2000, 100), HealthSample(4000, 120)))

    private fun json(walk: HealthWalk): String = JSONObject().apply {
        put("id", walk.id); put("startedMs", walk.startedMs); put("endedMs", walk.endedMs)
        put("timeS", walk.timeS); put("distanceM", walk.distanceM); put("steps", walk.steps)
        put("interrupted", walk.interrupted)
        put("heartRate", org.json.JSONArray(walk.samples.map {
            JSONObject().put("atMs", it.atMs).put("bpm", it.bpm).put("monitor", "private BLE identity")
        }))
    }.toString()

    private fun save(directory: File, walk: HealthWalk = walk()): File =
        File(directory, "walk-${walk.id}.json").apply { writeText(json(walk)) }

    @Test fun maps_walk_totals_and_real_heart_rate_to_walking_records() {
        val records = walk().copy(interrupted = true).records()
        val exercise = records.filterIsInstance<ExerciseSessionRecord>().single()
        assertEquals(ExerciseSessionRecord.EXERCISE_TYPE_WALKING, exercise.exerciseType)
        assertEquals(Instant.ofEpochMilli(1000), exercise.startTime)
        assertEquals(Instant.ofEpochMilli(10_000), exercise.endTime)
        assertTrue(exercise.title!!.contains("partial"))
        assertTrue(exercise.notes!!.contains("9 seconds"))
        assertEquals(12.0, records.filterIsInstance<DistanceRecord>().single().distance.inMeters, 0.0)
        assertEquals(20L, records.filterIsInstance<StepsRecord>().single().count)
        val samples = records.filterIsInstance<HeartRateRecord>().single().samples
        assertEquals(listOf(100L, 120L), samples.map { it.beatsPerMinute })
        assertEquals(listOf(2000L, 4000L), samples.map { it.time.toEpochMilli() })
        assertEquals(4, records.map { it.metadata.clientRecordId }.toSet().size)
        assertTrue(records.all { it.metadata.recordingMethod == Metadata.RECORDING_METHOD_AUTOMATICALLY_RECORDED })
        assertEquals(records.map { it.metadata.clientRecordId }, walk().records().map { it.metadata.clientRecordId })
    }

    @Test fun chunks_long_heart_rate_series_without_overlaps_duplicates_or_invalid_samples() {
        val w = walk().copy(samples = (0L..1200).reversed().map { HealthSample(1000 + it, 100) } + listOf(
            HealthSample(2000, 110), HealthSample(900, 90), HealthSample(10_000, 80),
            HealthSample(3000, 0), HealthSample(3001, 301)))
        val heart = w.records().filterIsInstance<HeartRateRecord>()
        assertEquals(listOf(500, 500, 201), heart.map { it.samples.size })
        val samples = heart.flatMap { it.samples }
        assertEquals(samples.sortedBy { it.time }, samples)
        assertEquals(110L, samples.single { it.time.toEpochMilli() == 2000L }.beatsPerMinute)
        assertEquals(1201, samples.map { it.time }.toSet().size)
        for (record in heart) assertTrue(record.samples.all { it.time >= record.startTime && it.time < record.endTime })
        assertTrue(heart.zipWithNext().all { (a, b) -> a.endTime == b.startTime })
    }

    @Test fun missing_metrics_are_omitted_and_invalid_intervals_rejected() {
        val empty = walk().copy(distanceM = 0, steps = 0, samples = emptyList()).records()
        assertEquals(1, empty.size)
        for (w in listOf(walk().copy(id = "../active"), walk().copy(endedMs = 1000),
            walk().copy(endedMs = 999), walk().copy(steps = -1))) {
            assertThrows(IllegalArgumentException::class.java) { w.records() }
        }
    }

    @Test fun reads_saved_json_and_rejects_unfinished_or_mismatched_walks() {
        assertEquals(walk(), HealthWalk.parse("1000-1", json(walk())))
        assertThrows(IllegalArgumentException::class.java) { HealthWalk.parse("1000-2", json(walk())) }
        val unfinished = JSONObject(json(walk())).put("endedMs", JSONObject.NULL)
        assertThrows(IllegalArgumentException::class.java) { HealthWalk.parse("1000-1", unfinished.toString()) }
    }

    @Test fun backfills_more_than_ten_walks_and_never_rewrites_receipted_copies() = runTest {
        val directory = temporary.newFolder()
        repeat(12) { save(directory, walk("1000-$it")) }
        File(directory, "active.json").writeText(json(walk("1000-99")))
        File(directory, "walk-1000-98.json.tmp").writeText(json(walk("1000-98")))
        var writes = 0
        val first = syncHealthWalks(directory, { true }) { writes += it.size }
        assertEquals(12, first.uploaded)
        assertEquals(48, writes)
        // Simulate deleting a synced record in Health Connect, and unreadable local content.
        // A later app launch must respect the receipt instead of recreating that data.
        File(directory, "walk-1000-1.json").writeText("damaged after sync")
        val second = syncHealthWalks(directory, { true }) { fail("Already exported walks were written twice") }
        assertEquals(0, second.uploaded)
        assertEquals(0, second.skipped)
        assertEquals(12, second.total)
    }

    @Test fun retry_after_partial_native_write_uses_same_ids_and_receipts_only_after_success() = runTest {
        val directory = temporary.newFolder()
        save(directory)
        val stored = mutableMapOf<String, Record>()
        try {
            syncHealthWalks(directory, { true }) { records ->
                records.forEach { stored[it.metadata.clientRecordId!!] = it }
                throw IOException("provider response lost after commit")
            }
            fail("Expected native failure")
        } catch (_: IOException) { }
        assertFalse(File(directory, "health-connect/1000-1.synced").exists())
        val retried = syncHealthWalks(directory, { true }) { records ->
            records.forEach { stored[it.metadata.clientRecordId!!] = it }
        }
        assertEquals(4, stored.size)
        assertEquals(1, retried.uploaded)
        assertTrue(File(directory, "health-connect/1000-1.synced").isFile)
    }

    @Test fun corrupt_walk_does_not_block_other_walks_and_disabled_sync_does_not_write() = runTest {
        val directory = temporary.newFolder()
        save(directory)
        File(directory, "walk-1000-2.json").writeText("{")
        save(directory, walk("1000-3").copy(endedMs = 1000))
        val disabled = syncHealthWalks(directory, { false }) { fail("Sync was disabled") }
        assertEquals(0, disabled.uploaded)
        val report = syncHealthWalks(directory, { true }) { }
        assertEquals(1, report.uploaded)
        assertEquals(2, report.skipped)
    }

    @Test fun cancellation_and_permission_revocation_never_acknowledge_a_walk() = runTest {
        for (failure in listOf(CancellationException("stopped"), SecurityException("revoked"))) {
            val directory = temporary.newFolder()
            save(directory)
            try { syncHealthWalks(directory, { true }) { throw failure }; fail("Expected stop") }
            catch (error: Exception) { assertSame(failure, error) }
            assertFalse(File(directory, "health-connect/1000-1.synced").exists())
        }
    }

    @Test fun large_walk_stops_between_batches_and_can_resume_without_duplicate_ids() = runTest {
        val directory = temporary.newFolder()
        val longWalk = walk().copy(endedMs = 100_000,
            samples = (0L..50_000).map { HealthSample(1000 + it, 100) })
        save(directory, longWalk)
        var enabled = true
        val stored = mutableMapOf<String, Record>()
        val interrupted = syncHealthWalks(directory, { enabled }) { records ->
            assertTrue(records.size <= 100)
            records.forEach { stored[it.metadata.clientRecordId!!] = it }
            enabled = false
        }
        assertEquals(0, interrupted.uploaded)
        assertEquals(100, stored.size)
        assertFalse(File(directory, "health-connect/1000-1.synced").exists())
        val resumed = syncHealthWalks(directory, { true }) { records ->
            assertTrue(records.size <= 100)
            records.forEach { stored[it.metadata.clientRecordId!!] = it }
        }
        assertEquals(1, resumed.uploaded)
        assertEquals(longWalk.records().size, stored.size)
    }

    @Test fun provider_rejected_walk_does_not_block_valid_walks() = runTest {
        val directory = temporary.newFolder()
        save(directory)
        save(directory, walk("1000-2"))
        val report = syncHealthWalks(directory, { true }) { records ->
            if (records.first().metadata.clientRecordId!!.contains(":1000-1:")) throw IllegalArgumentException("rejected")
        }
        assertEquals(1, report.uploaded)
        assertEquals(1, report.skipped)
        assertFalse(File(directory, "health-connect/1000-1.synced").exists())
        assertTrue(File(directory, "health-connect/1000-2.synced").isFile)
    }
}
