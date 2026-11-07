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
import uniffi.threnody_ffi.callsBuiltIn

/**
 * Keeps the process, and so the node, alive while the app is in the
 * background: sessions stay up and incoming messages raise notifications.
 *
 * With UnifiedPush wake-ups on ([Push]), it runs only briefly instead: in
 * wake mode, to sync after a push, or during a call.
 */
class ThrenodyService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        running = this
        if (intent?.action == ACTION_WAKE) {
            wake()
            return START_NOT_STICKY
        }
        // Woken by pushes, the app sleeps: send what's due, then sleep again.
        // Otherwise the node runs on, and sends scheduled messages itself.
        if (intent?.action == ACTION_SCHEDULED && Push.active(this)) {
            scheduledWake()
            return START_NOT_STICKY
        }
        foreground(intent?.action == ACTION_CALL || Calls.current != null)
        // Restarted by the system without the Activity: bring the node up here.
        Thread { Threnody.start(applicationContext) }.start()
        // Woken by pushes: never restarted by the system just to stay connected.
        return if (Push.active(this)) START_NOT_STICKY else START_STICKY
    }

    /** Woken by a push: sync, then sleep again unless the app came up meanwhile. */
    private fun wake() = briefly("Checking for messages") { Push.sync(applicationContext) }

    /** Woken by the scheduled-message alarm, with push wake-ups on. */
    private fun scheduledWake() = briefly("Sending scheduled messages") {
        Scheduled.sendDue(applicationContext, Scheduled.SERVICE_BUDGET_MS)
    }

    /**
     * Runs [work] in the foreground under a brief notification saying
     * [title], then sleeps again unless the app came up meanwhile.
     */
    private fun briefly(title: String, work: () -> Unit) {
        channels(this)
        val n = Notification.Builder(this, CHANNEL_WAKE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentIntent(openApp(this))
            .build()
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(ID_SERVICE, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING)
            } else {
                startForeground(ID_SERVICE, n)
            }
        } catch (e: Exception) {
            Threnody.say("! wake: foreground refused (${e.message})")
        }
        Thread {
            work()
            if (Threnody.visible > 0 || Calls.current != null) {
                // In use now: in front no service is needed; in a call, the call's.
                android.os.Handler(android.os.Looper.getMainLooper()).post {
                    if (Calls.current != null) foreground(true) else stopSelf()
                }
            } else {
                Push.sleepIfIdle(applicationContext)
                // Not allowed to sleep (tunnels, say): at least drop the notification.
                android.os.Handler(android.os.Looper.getMainLooper()).post { stopSelf() }
            }
        }.start()
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
            // A build without calls declares no microphone type.
            if (call && callsBuiltIn() && Calls.hasMicrophone(this)) {
                types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            }
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
        private const val CHANNEL_WAKE = "wake"
        private const val ID_SERVICE = 1
        private const val ID_MESSAGE = 2
        /** What a notification says when it may not say more. */
        private const val NEW = "New message"

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, ThrenodyService::class.java))
        }

        private const val ACTION_CALL = "org.threnody.app.IN_CALL"
        /** Started by a UnifiedPush wake-up ([Push]). */
        const val ACTION_WAKE = "org.threnody.app.WAKE"
        /** Started by the scheduled-message alarm or at boot ([Scheduled]). */
        const val ACTION_SCHEDULED = "org.threnody.app.SCHEDULED"
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

        /**
         * Shows an incoming message while its chat isn't on screen. A
         * [disappearing] message's text is left out: the notification
         * would outlive it.
         */
        fun notifyMessage(
            ctx: Context, key: String, device: String, from: String, text: String, persona: String? = null,
            disappearing: Boolean = false,
        ) {
            val open = Intent(ctx, ChatActivity::class.java)
                .putExtra(ChatActivity.KEY, key)
                .putExtra(ChatActivity.DEVICE, device)
                .putExtra(ChatActivity.PERSONA, persona)
            post(ctx, notificationKey(key, persona), titled(from, persona), if (disappearing) NEW else text, open)
        }

        /**
         * A scheduled message to the chat [key] wasn't sent, and why (never
         * its text). Its own notification, so it neither replaces nor is
         * replaced by the chat's messages.
         */
        fun notifyScheduledFailed(
            ctx: Context, key: String, device: String?, group: String?, title: String, reason: String,
            persona: String? = null,
        ) {
            val open = Intent(ctx, ChatActivity::class.java).putExtra(ChatActivity.PERSONA, persona)
            if (group != null) open.putExtra(ChatActivity.GROUP, group)
            else open.putExtra(ChatActivity.KEY, key).putExtra(ChatActivity.DEVICE, device)
            post(ctx, "scheduled:" + notificationKey(key, persona), titled(title, persona),
                "Scheduled message not sent: $reason", open)
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

        /**
         * One notification per conversation [key], opening [open] above the
         * list. With private notifications (on unless turned off) it says
         * only that something arrived, not who or what, wherever it shows.
         */
        private fun post(ctx: Context, key: String, title: String, text: String, open: Intent) {
            channels(ctx)
            // With app lock on, one notification for everything, saying
            // nothing and opening only the app: not even how many wrote.
            if (AppLock.enabled(ctx)) return postLocked(ctx)
            val hidden = Privacy.privateNotifications(ctx)
            // On the lock screen: only that something arrived, not who or what.
            val public = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle("Threnody")
                .setContentText(NEW)
                .build()
            val n = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(if (hidden) "Threnody" else title)
                .setContentText(if (hidden) NEW else text)
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

        /** The tag of the one notification shown while app lock is on. */
        private const val LOCKED_TAG = "message"

        private fun postLocked(ctx: Context) {
            val n = Notification.Builder(ctx, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle("Threnody")
                .setContentText(NEW)
                .setVisibility(Notification.VISIBILITY_SECRET)
                .setContentIntent(PendingIntent.getActivity(ctx, LOCKED_TAG.hashCode(),
                    Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE))
                .setAutoCancel(true)
                .build()
            try {
                ctx.getSystemService(NotificationManager::class.java)?.notify(LOCKED_TAG, ID_MESSAGE, n)
            } catch (_: SecurityException) {
            }
        }

        /** A group message or invitation; opens the group. As for [notifyMessage]. */
        fun notifyGroup(
            ctx: Context, group: String, title: String, text: String, persona: String? = null,
            disappearing: Boolean = false,
        ) {
            val open = Intent(ctx, ChatActivity::class.java)
                .putExtra(ChatActivity.GROUP, group)
                .putExtra(ChatActivity.PERSONA, persona)
            post(ctx, notificationKey(group, persona), titled(title, persona), if (disappearing) NEW else text, open)
        }

        /** The app was opened: the one notification shown while locked goes. */
        fun clearLocked(ctx: Context) {
            ctx.getSystemService(NotificationManager::class.java)?.cancel(LOCKED_TAG, ID_MESSAGE)
        }

        fun clearNotification(ctx: Context, key: String) {
            val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
            nm.cancel(key, ID_MESSAGE)
            nm.cancel(LOCKED_TAG, ID_MESSAGE)
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
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_WAKE, "Checking for messages", NotificationManager.IMPORTANCE_MIN)
                    .apply { setShowBadge(false) }
            )
        }

        private fun openApp(ctx: Context): PendingIntent = PendingIntent.getActivity(
            ctx, 0,
            Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
    }
}
