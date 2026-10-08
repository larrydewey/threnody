package org.threnody.app

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Person
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.RingtoneManager
import android.os.Build
import android.os.SystemClock
import android.widget.Toast
import uniffi.threnody_ffi.NodeEvent
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Voice calls (Appendix Q). The node runs the call and its audio (WebRTC
 * on the microphone and earpiece or speaker); this keeps track of the one
 * call, routes its audio, rings for incoming ones and opens [CallActivity].
 */
object Calls {
    /** The call in progress, as the screen shows it. */
    data class Call(
        val node: ThrenodyNode,
        val persona: String?,
        val id: ULong,
        val peer: String,
        val title: String,
        val outgoing: Boolean,
        /** "calling", "ringing", "incoming", "connecting", "connected", "interrupted". */
        val state: String,
        /** When audio connected ([SystemClock.elapsedRealtime]), 0 before. */
        val since: Long = 0,
        val muted: Boolean = false,
        val speaker: Boolean = false,
    )

    @Volatile var current: Call? = null
        private set

    /** Why the last call ended, for the screen to show as it closes. */
    @Volatile var ended: String? = null
        private set

    private val listeners = java.util.concurrent.CopyOnWriteArrayList<() -> Unit>()

    fun listen(l: () -> Unit): () -> Unit {
        listeners.add(l)
        return { listeners.remove(l) }
    }

    private fun changed() = listeners.forEach { it() }

    /**
     * Hands WebRTC what it needs before the node opens: loading the library
     * through Android (not only JNA) runs WebRTC's own JNI_OnLoad, which
     * keeps the JavaVM; ContextUtils gives its audio the app context.
     */
    fun prepare(ctx: Context) {
        try {
            System.loadLibrary("threnody_ffi")
            livekit.org.webrtc.ContextUtils.initialize(ctx.applicationContext)
        } catch (e: Throwable) {
            Threnody.say("! calls unavailable: ${e.message}")
        }
    }

    fun hasMicrophone(ctx: Context) =
        ctx.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    /** Calls a contact (asking for the microphone first if need be). */
    fun start(a: Activity, node: ThrenodyNode, persona: String?, device: String, title: String) {
        if (current != null) return Toast.makeText(a, "You're already in a call.", Toast.LENGTH_SHORT).show()
        if (!hasMicrophone(a)) {
            pendingCall = { start(a, node, persona, device, title) }
            return a.requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), MICROPHONE)
        }
        Threading.background {
            val id = try { node.startCall(device, false) } catch (e: Exception) {
                return@background a.runOnUiThread {
                    Toast.makeText(a, "Couldn't call $title: ${e.message}", Toast.LENGTH_LONG).show()
                }
            }
            ended = null
            current = Call(node, persona, id, device, title, outgoing = true, state = "calling")
            inCall(a, true)
            changed()
            a.startActivity(Intent(a, CallActivity::class.java))
        }
    }

    /** A call waiting on the microphone permission. */
    private var pendingCall: (() -> Unit)? = null
    const val MICROPHONE = 7

    /** From an activity's onRequestPermissionsResult. */
    fun permissionResult(code: Int, results: IntArray) {
        if (code != MICROPHONE) return
        val run = pendingCall
        pendingCall = null
        if (results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }) run?.invoke()
    }

    fun answer(a: Activity) {
        val c = current?.takeIf { it.state == "incoming" } ?: return
        if (!hasMicrophone(a)) {
            pendingCall = { answer(a) }
            return a.requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), MICROPHONE)
        }
        cancelRinging(a)
        Threading.background {
            try {
                c.node.answerCall(c.id, false)
                current = c.copy(state = "connecting")
                inCall(a, true)
            } catch (e: Exception) {
                Threnody.say("! answer: ${e.message}")
            }
            changed()
        }
    }

    /** Hangs up, declines or cancels the current call. */
    fun hangUp(ctx: Context) {
        val c = current ?: return
        Threading.background { c.node.hangupCall(c.id) }
        cancelRinging(ctx)
    }

    fun setMuted(muted: Boolean) {
        val c = current ?: return
        current = c.copy(muted = muted)
        Threading.background { c.node.setCallMuted(muted) }
        changed()
    }

    fun setSpeaker(ctx: Context, on: Boolean) {
        val c = current ?: return
        current = c.copy(speaker = on)
        route(ctx, on)
        changed()
    }

    /** Call events from a node's event thread (the main identity's or a persona's). */
    fun onEvent(ctx: Context, node: ThrenodyNode, persona: String?, e: NodeEvent) {
        when (e) {
            is NodeEvent.CallIncoming -> {
                ended = null
                current = Call(node, persona, e.call, e.peer, Threnody.nameOf(node, e.peer), outgoing = false, state = "incoming")
                ring(ctx)
            }
            is NodeEvent.CallRinging -> update(e.call) { it.copy(state = "ringing") }
            is NodeEvent.CallStarted -> update(e.call) { it.copy(state = "connecting") }
            is NodeEvent.CallMedia -> update(e.call) {
                when (e.state) {
                    "connected" -> it.copy(state = "connected", since = if (it.since == 0L) SystemClock.elapsedRealtime() else it.since)
                    "interrupted" -> it.copy(state = "interrupted")
                    else -> it
                }
            }
            is NodeEvent.CallEnded -> {
                val c = current?.takeIf { it.id == e.call } ?: return
                current = null
                cancelRinging(ctx)
                inCall(ctx, false)
                ended = when {
                    e.reason == "declined" && !e.byUs -> "Declined"
                    e.reason == "busy" -> "Busy"
                    e.reason == "unanswered" && !e.byUs -> "No answer"
                    e.reason == "failed" -> "Call failed"
                    c.since == 0L && !c.outgoing && !e.byUs -> "Missed call"
                    else -> "Call ended"
                }
                if (ended == "Missed call") missed(ctx, c)
            }
            else -> return
        }
        changed()
    }

    private fun update(id: ULong, f: (Call) -> Call) {
        current?.takeIf { it.id == id }?.let { current = f(it) }
    }

    /** Voice-call audio mode while a call runs (echo cancellation, earpiece). */
    private fun inCall(ctx: Context, on: Boolean) {
        val am = ctx.getSystemService(AudioManager::class.java) ?: return
        am.mode = if (on) AudioManager.MODE_IN_COMMUNICATION else AudioManager.MODE_NORMAL
        if (!on) route(ctx, false)
    }

    /** Speaker or earpiece. */
    private fun route(ctx: Context, speaker: Boolean) {
        val am = ctx.getSystemService(AudioManager::class.java) ?: return
        if (Build.VERSION.SDK_INT >= 31) {
            if (!speaker) return am.clearCommunicationDevice()
            am.availableCommunicationDevices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER }
                ?.let { am.setCommunicationDevice(it) }
        } else {
            @Suppress("DEPRECATION")
            am.isSpeakerphoneOn = speaker
        }
    }

    // ----- Notifications -----

    /** Rings with the phone's ringtone, as a ringtone (silent mode and Do Not Disturb apply). */
    private const val CHANNEL_RING = "calls_ring"
    private const val CHANNEL_MISSED = "calls_missed"
    private const val ID_RINGING = 30
    private const val ID_MISSED = 31

    private fun channels(ctx: Context): NotificationManager {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        // The first test builds' channel, which only chimed.
        nm.deleteNotificationChannel("calls")
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_RING, "Incoming calls", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "Rings for an incoming call"
                setSound(
                    RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE),
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build(),
                )
                enableVibration(true)
                vibrationPattern = longArrayOf(0, 1000, 1000)
            },
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_MISSED, "Missed calls", NotificationManager.IMPORTANCE_DEFAULT),
        )
        return nm
    }

    /** Rings: a full-screen call notification (the screen itself when unlocked and in use). */
    private fun ring(ctx: Context) {
        val c = current ?: return
        val open = PendingIntent.getActivity(
            ctx, 0, Intent(ctx, CallActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val answer = PendingIntent.getActivity(
            ctx, 1,
            Intent(ctx, CallActivity::class.java).setAction(CallActivity.ANSWER).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val decline = PendingIntent.getBroadcast(
            ctx, 2, Intent(ctx, Decline::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = Notification.Builder(ctx, CHANNEL_RING)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("${c.title} is calling")
            .setContentText("Threnody voice call")
            .apply {
                // A call notification (Android 12+): the system rings it as
                // a call, with Answer and Decline (on the lock screen too).
                if (Build.VERSION.SDK_INT >= 31) {
                    setStyle(Notification.CallStyle.forIncomingCall(Person.Builder().setName(c.title).build(), decline, answer))
                }
            }
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(public(ctx, "Incoming call"))
            .setCategory(Notification.CATEGORY_CALL)
            .setFullScreenIntent(open, true)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        // Rings (and vibrates) over and over until answered or declined.
        n.flags = n.flags or Notification.FLAG_INSISTENT
        post(ctx, ID_RINGING, n)
    }

    /** Decline, from the ringing notification. */
    class Decline : BroadcastReceiver() {
        override fun onReceive(ctx: Context, intent: Intent) = hangUp(ctx)
    }

    private fun cancelRinging(ctx: Context) {
        ctx.getSystemService(NotificationManager::class.java)?.cancel(ID_RINGING)
    }

    private fun missed(ctx: Context, c: Call) {
        val n = Notification.Builder(ctx, CHANNEL_MISSED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("Missed call from ${c.title}")
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(public(ctx, "Missed call"))
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_MISSED_CALL)
            .build()
        post(ctx, ID_MISSED, n)
    }

    /** On the lock screen: only that there's a call, not who. */
    private fun public(ctx: Context, text: String) = Notification.Builder(ctx, CHANNEL_MISSED)
        .setSmallIcon(R.drawable.ic_notification)
        .setContentTitle("Threnody")
        .setContentText(text)
        .build()

    private fun post(ctx: Context, id: Int, n: Notification) {
        try {
            channels(ctx).notify(id, n)
        } catch (_: SecurityException) {
            // Notifications not permitted: the call screen still opens in the app.
        }
    }
}
