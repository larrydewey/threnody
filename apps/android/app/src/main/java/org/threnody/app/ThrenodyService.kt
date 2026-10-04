package org.threnody.app

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

/**
 * Keeps the process, and so the node, alive while the app is in the
 * background: sessions stay up and incoming messages raise notifications.
 */
class ThrenodyService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        channels(this)
        val n = Notification.Builder(this, CHANNEL_SERVICE)
            .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
            .setContentTitle("Threnody is running")
            .setContentText("Connected peers can reach you.")
            .setContentIntent(openApp(this))
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(ID_SERVICE, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING)
        } else {
            startForeground(ID_SERVICE, n)
        }
        // Restarted by the system without the Activity: bring the node up here.
        Thread { Threnody.start(applicationContext) }.start()
        return START_STICKY
    }

    companion object {
        private const val CHANNEL_SERVICE = "service"
        private const val CHANNEL_MESSAGES = "messages"
        private const val ID_SERVICE = 1
        private const val ID_MESSAGE = 2

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, ThrenodyService::class.java))
        }

        /** Shows an incoming message while no Activity is in front. */
        fun notifyMessage(ctx: Context, from: String, text: String) {
            channels(ctx)
            val n = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(android.R.drawable.stat_notify_chat)
                .setContentTitle(from)
                .setContentText(text)
                .setContentIntent(openApp(ctx))
                .setAutoCancel(true)
                .build()
            try {
                ctx.getSystemService(NotificationManager::class.java)?.notify(ID_MESSAGE, n)
            } catch (_: SecurityException) {
                // Notifications not permitted; the message is still in history.
            }
        }

        private fun channels(ctx: Context) {
            val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_SERVICE, "Background connection", NotificationManager.IMPORTANCE_LOW)
            )
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_MESSAGES, "Messages", NotificationManager.IMPORTANCE_HIGH)
            )
        }

        private fun openApp(ctx: Context): PendingIntent = PendingIntent.getActivity(
            ctx, 0,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
    }
}
