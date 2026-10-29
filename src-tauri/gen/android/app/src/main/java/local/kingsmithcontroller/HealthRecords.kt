package local.kingsmithcontroller

import androidx.health.connect.client.records.DistanceRecord
import androidx.health.connect.client.records.ExerciseSessionRecord
import androidx.health.connect.client.records.HeartRateRecord
import androidx.health.connect.client.records.Record
import androidx.health.connect.client.records.StepsRecord
import androidx.health.connect.client.records.metadata.Device
import androidx.health.connect.client.records.metadata.Metadata
import androidx.health.connect.client.units.Length
import org.json.JSONObject
import java.time.Instant

internal data class HealthSample(val atMs: Long, val bpm: Long)

internal data class HealthWalk(
    val id: String,
    val startedMs: Long,
    val endedMs: Long,
    val timeS: Long,
    val distanceM: Long,
    val steps: Long,
    val interrupted: Boolean,
    val samples: List<HealthSample>,
) {
    fun records(): List<Record> {
        require(ID.matches(id)) { "Invalid walk ID" }
        require(startedMs >= 0 && endedMs > startedMs) { "Walk has no positive interval" }
        require(timeS >= 0 && distanceM >= 0 && steps >= 0) { "Invalid walk totals" }
        val start = Instant.ofEpochMilli(startedMs)
        val end = Instant.ofEpochMilli(endedMs)
        val pad = Device(type = Device.TYPE_UNKNOWN, manufacturer = "KingSmith", model = "WalkingPad R1 Pro")
        fun metadata(kind: String, device: Device = pad) = Metadata.autoRecorded(
            device = device, clientRecordId = "kingsmith:walk:$id:$kind", clientRecordVersion = 1,
        )
        val records = mutableListOf<Record>(ExerciseSessionRecord(
            startTime = start, startZoneOffset = null, endTime = end, endZoneOffset = null,
            exerciseType = ExerciseSessionRecord.EXERCISE_TYPE_WALKING,
            title = if (interrupted) "WalkingPad walk (partial)" else "WalkingPad walk",
            notes = "Observed moving time: $timeS seconds." + if (interrupted) " Pad status was lost or the app closed." else "",
            metadata = metadata("exercise"),
        ))
        // Zero counters can mean missing pad data. Do not turn missing data into inactivity.
        if (distanceM > 0) records += DistanceRecord(
            startTime = start, startZoneOffset = null, endTime = end, endZoneOffset = null,
            distance = Length.meters(distanceM.toDouble()), metadata = metadata("distance"),
        )
        if (steps > 0) records += StepsRecord(
            startTime = start, startZoneOffset = null, endTime = end, endZoneOffset = null,
            count = steps, metadata = metadata("steps"),
        )
        // Stable, bounded chunks allow long walks and safe retries after a partial batch.
        // Keep actual sample times; never interpolate across a Bluetooth gap.
        val chunks = samples.filter { it.atMs in startedMs until endedMs && it.bpm in 1..300 }
            .associateBy { it.atMs }.toSortedMap().values.chunked(500)
        chunks.forEachIndexed { index, chunk ->
                records += HeartRateRecord(
                    startTime = if (index == 0) start else Instant.ofEpochMilli(chunk.first().atMs),
                    startZoneOffset = null,
                    endTime = chunks.getOrNull(index + 1)?.first()?.atMs?.let(Instant::ofEpochMilli) ?: end,
                    endZoneOffset = null,
                    samples = chunk.map { HeartRateRecord.Sample(Instant.ofEpochMilli(it.atMs), it.bpm) },
                    metadata = metadata("heart-$index", Device(type = Device.TYPE_UNKNOWN, model = "Bluetooth heart-rate monitor")),
                )
            }
        return records
    }

    companion object {
        val ID = Regex("[0-9]+-[0-9]+")
        val FILE = Regex("walk-([0-9]+-[0-9]+)\\.json")

        fun parse(id: String, json: String): HealthWalk {
            val data = JSONObject(json)
            require(data.getString("id") == id && !data.isNull("endedMs")) { "Walk is not saved" }
            val readings = data.getJSONArray("heartRate")
            return HealthWalk(id, data.getLong("startedMs"), data.getLong("endedMs"), data.getLong("timeS"),
                data.getLong("distanceM"), data.getLong("steps"), data.getBoolean("interrupted"),
                (0 until readings.length()).map { index -> readings.getJSONObject(index).let {
                    HealthSample(it.getLong("atMs"), it.getLong("bpm"))
                } })
        }
    }
}
