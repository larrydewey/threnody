package org.threnody.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.app.TaskStackBuilder
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
        running = this
        foreground(intent?.action == ACTION_CALL || Calls.current != null)
        // Restarted by the system without the Activity: bring the node up here.
        Thread { Threnody.start(applicationContext) }.start()
        return START_STICKY
    }

    override fun onDestroy() {
        if (running === this) running = null
        super.onDestroy()
    }

    /**
     * In a call, the service holds the microphone too: without that, Android
     * cuts a backgrounded app off from it, and the call fails.
     */
    private fun foreground(call: Boolean) {
        channels(this)
        // In a call, the call's own notification (back to it, Hang up).
        val n = Calls.callNotification(this).takeIf { call } ?: Notification.Builder(this, CHANNEL_SERVICE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("Threnody is running")
            .setContentText("Connected peers can reach you.")
            .setContentIntent(openApp(this))
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= 34) {
            var types = ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
            if (call && Calls.hasMicrophone(this)) types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            try {
                startForeground(ID_SERVICE, n, types)
            } catch (e: Exception) {
                // Not allowed the microphone just now (in the background): carry on without.
                startForeground(ID_SERVICE, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING)
            }
        } else {
            startForeground(ID_SERVICE, n)
        }
    }

    companion object {
        private const val CHANNEL_SERVICE = "service2" // showBadge=false requires a fresh channel
        private const val CHANNEL_MESSAGES = "messages"
        private const val ID_SERVICE = 1
        private const val ID_MESSAGE = 2

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, ThrenodyService::class.java))
        }

        private const val ACTION_CALL = "org.threnody.app.IN_CALL"
        @Volatile private var running: ThrenodyService? = null

        /**
         * A call started (`on`, while the app is in front, as Android
         * requires for the microphone) or ended: the service holds the
         * microphone, or lets go.
         */
        fun inCall(ctx: Context, on: Boolean) {
            val s = running
            if (s != null) {
                android.os.Handler(android.os.Looper.getMainLooper()).post { s.foreground(on) }
            } else if (on) {
                ctx.startForegroundService(Intent(ctx, ThrenodyService::class.java).setAction(ACTION_CALL))
            }
        }

        /** Shows an incoming message while its chat isn't on screen. */
        fun notifyMessage(ctx: Context, key: String, device: String, from: String, text: String, persona: String? = null) {
            val open = Intent(ctx, ChatActivity::class.java)
                .putExtra(ChatActivity.KEY, key)
                .putExtra(ChatActivity.DEVICE, device)
                .putExtra(ChatActivity.PERSONA, persona)
            post(ctx, notificationKey(key, persona), titled(from, persona), text, open)
        }

        /** About this device's account (it was removed from it); opens the app. */
        fun notifyAccount(ctx: Context, text: String) {
            channels(ctx)
            val n = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle("Threnody")
                .setContentText(text)
                .setStyle(Notification.BigTextStyle().bigText(text))
                .setContentIntent(PendingIntent.getActivity(ctx, "account".hashCode(),
                    Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE))
                .setAutoCancel(true)
                .build()
            try {
                ctx.getSystemService(NotificationManager::class.java)?.notify("account", ID_MESSAGE, n)
            } catch (_: SecurityException) {
            }
        }

        /** Conversations of different identities never share a notification. */
        fun notificationKey(key: String, persona: String?) = if (persona == null) key else "$persona:$key"

        /** Which anonymous identity a notification is for, by its label. */
        private fun titled(title: String, persona: String?): String =
            if (persona == null) title else "🎭 ${Threnody.personaLabels[persona] ?: "Anonymous"} · $title"

        /** One notification per conversation [key], opening [open] above the list. */
        private fun post(ctx: Context, key: String, title: String, text: String, open: Intent) {
            channels(ctx)
            // On the lock screen: only that something arrived, not who or what.
            val public = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle("Threnody")
                .setContentText("New message")
                .build()
            val n = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(title)
                .setContentText(text)
                .setVisibility(Notification.VISIBILITY_PRIVATE)
                .setPublicVersion(public)
                .setContentIntent(
                    // Back from the chat leads to the conversation list.
                    TaskStackBuilder.create(ctx)
                        .addNextIntent(Intent(ctx, MainActivity::class.java))
                        .addNextIntent(open)
                        .getPendingIntent(key.hashCode(), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT),
                )
                .setAutoCancel(true)
                .build()
            try {
                ctx.getSystemService(NotificationManager::class.java)?.notify(key, ID_MESSAGE, n)
            } catch (_: SecurityException) {
                // Notifications not permitted; the message is still in history.
            }
        }

        /** A group message or invitation; opens the group. */
        fun notifyGroup(ctx: Context, group: String, title: String, text: String, persona: String? = null) {
            val open = Intent(ctx, ChatActivity::class.java)
                .putExtra(ChatActivity.GROUP, group)
                .putExtra(ChatActivity.PERSONA, persona)
            post(ctx, notificationKey(group, persona), titled(title, persona), text, open)
        }

        fun clearNotification(ctx: Context, key: String) {
            ctx.getSystemService(NotificationManager::class.java)?.cancel(key, ID_MESSAGE)
        }

        private fun channels(ctx: Context) {
            val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_SERVICE, "Background connection", NotificationManager.IMPORTANCE_LOW)
                    .apply { setShowBadge(false) }
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
