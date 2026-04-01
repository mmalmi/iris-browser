package to.iris.browser.mobilebluetooth

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat

internal class MobileBluetoothForegroundService : Service() {
    companion object {
        private const val CHANNEL_ID = "iris_mobile_bluetooth"
        private const val NOTIFICATION_ID = 10401
        private const val ACTION_START = "to.iris.browser.mobilebluetooth.action.START"
        private const val ACTION_STOP = "to.iris.browser.mobilebluetooth.action.STOP"

        fun start(context: Context) {
            val appContext = context.applicationContext
            val intent = Intent(appContext, MobileBluetoothForegroundService::class.java).apply {
                action = ACTION_START
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                appContext.startForegroundService(intent)
            } else {
                appContext.startService(intent)
            }
        }

        fun stop(context: Context) {
            val appContext = context.applicationContext
            appContext.stopService(Intent(appContext, MobileBluetoothForegroundService::class.java))
        }
    }

    override fun onCreate() {
        super.onCreate()
        Log.i("MobileBluetoothFG", "foreground service created")
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i("MobileBluetoothFG", "foreground service start command action=${intent?.action}")
        if (intent?.action == ACTION_STOP) {
            stopForegroundCompat()
            stopSelf()
            return START_NOT_STICKY
        }

        startForegroundCompat(buildNotification())
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        Log.i("MobileBluetoothFG", "foreground service destroyed")
        stopForegroundCompat()
        super.onDestroy()
    }

    private fun buildNotification(): Notification {
        val appName = applicationInfo.loadLabel(packageManager).toString().ifBlank { "Iris" }
        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)?.apply {
            addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        }
        val pendingIntent =
            launchIntent?.let {
                PendingIntent.getActivity(
                    this,
                    0,
                    it,
                    PendingIntent.FLAG_UPDATE_CURRENT or
                        (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                            PendingIntent.FLAG_IMMUTABLE
                        } else {
                            0
                        }),
                )
            }

        val builder =
            NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
                .setContentTitle(appName)
                .setContentText("Bluetooth mesh stays active while Iris is backgrounded.")
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .setCategory(NotificationCompat.CATEGORY_SERVICE)

        if (pendingIntent != null) {
            builder.setContentIntent(pendingIntent)
        }

        return builder.build()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }

        val channel =
            NotificationChannel(
                CHANNEL_ID,
                "Bluetooth background mesh",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps Iris Bluetooth mesh active while the app is backgrounded."
                setShowBadge(false)
            }
        (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
            .createNotificationChannel(channel)
    }

    private fun startForegroundCompat(notification: Notification) {
        if (Build.VERSION.SDK_INT >= 34) {
            try {
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
                )
                Log.i("MobileBluetoothFG", "foreground notification started with connectedDevice type")
                return
            } catch (error: SecurityException) {
                Log.w(
                    "MobileBluetoothFG",
                    "Connected-device foreground type rejected, retrying without explicit type",
                    error,
                )
            }
        }
        startForeground(NOTIFICATION_ID, notification)
        Log.i("MobileBluetoothFG", "foreground notification started")
    }

    private fun stopForegroundCompat() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_REMOVE)
        } else {
            @Suppress("DEPRECATION")
            stopForeground(true)
        }
    }
}
