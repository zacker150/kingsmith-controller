package local.kingsmithcontroller

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log

/** Keeps the Rust BLE tasks schedulable while the screen is off. */
class ConnectionService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private var wakeLock: PowerManager.WakeLock? = null
    private val renew = object : Runnable {
        override fun run() {
            // A timed lock also expires if this renewal loop stops unexpectedly.
            wakeLock?.acquire(10 * 60 * 1000L)
            handler.postDelayed(this, 5 * 60 * 1000L)
        }
    }

    override fun onCreate() {
        super.onCreate()
        val notifications = getSystemService(NotificationManager::class.java)
        notifications.createNotificationChannel(NotificationChannel(
            CHANNEL, "Bluetooth connections", NotificationManager.IMPORTANCE_LOW
        ))
        val open = PendingIntent.getActivity(this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_bluetooth_connection)
            .setContentTitle("Kingsmith Controller connected")
            .setContentText("Keeping Bluetooth and walk recording active. Tap to open.")
            .setContentIntent(open)
            .setOngoing(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .build()
        startForeground(NOTIFICATION, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        wakeLock = getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "kingsmith:connections")
            .apply { setReferenceCounted(false) }
        renew.run()
        Log.i("KSCTRL", "background: connected-device service started; CPU lock acquired")
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        handler.removeCallbacks(renew)
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        stopForeground(STOP_FOREGROUND_REMOVE)
        Log.i("KSCTRL", "background: connection service stopped; CPU lock released")
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "bluetooth-connections"
        private const val NOTIFICATION = 1001
    }
}
