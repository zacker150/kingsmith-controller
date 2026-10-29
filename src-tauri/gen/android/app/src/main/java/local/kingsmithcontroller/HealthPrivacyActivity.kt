package local.kingsmithcontroller

import android.app.Activity
import android.os.Bundle
import android.widget.ScrollView
import android.widget.TextView

/** Available from both Android 13 and Android 14+ Health Connect permission screens. */
class HealthPrivacyActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = "Health Connect privacy"
        val text = TextView(this).apply {
            textSize = 18f
            val padding = (24 * resources.displayMetrics.density).toInt()
            setPadding(padding, padding, padding, padding)
            text = """
                Health Connect and your walks

                When you enable automatic sync, Kingsmith Controller writes your saved walking workouts, distance, steps, and recorded heart rate to Health Connect on this phone. Existing saved walks are included. Partial walks are identified as partial.

                The app requests write access only. It does not read other apps' health data or send your walks to a server. Apps you authorize in Health Connect can access the records according to their permissions.

                You can turn automatic sync off in Kingsmith Controller or revoke permissions in Health Connect. Previously exported records stay in Health Connect. Delete those copies using Health Connect's data controls; deleting a local walk does not delete its Health Connect copy.

                Saved walks and export receipts stay in this app's private storage until you delete the local data or uninstall the app. Health Connect manages the retention of exported records.
            """.trimIndent()
        }
        setContentView(ScrollView(this).apply { addView(text) })
    }
}
