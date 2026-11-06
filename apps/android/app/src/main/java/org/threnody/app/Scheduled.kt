package org.threnody.app

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import java.io.File
import uniffi.threnody_ffi.ScheduleFailure
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Scheduled messages ("send later"): the node sends them when due. This
 * keeps an alarm set for the next one, so a phone asleep (or a process
 * ended by UnifiedPush sleep, [Push]) wakes up to send it, and brings the
 * nodes up for it after a reboot.
 *
 * Nothing is kept here: whether anything is pending is whether a node's
 * encrypted `scheduled.state` exists (the node deletes it once nothing is
 * scheduled), and the next time lives in the nodes and the alarm.
 */
object Scheduled {
    /** A send for an alarm or a boot is running: don't sleep under it. */
    @Volatile var sending = false

    /** Some identity has messages scheduled (its schedule file exists). */
    fun pendingOnDisk(ctx: Context): Boolean {
        val home = File(Threnody.home(ctx))
        if (File(home, "scheduled.state").exists()) return true
        return File(home, "personas").listFiles()?.any { File(it, "scheduled.state").exists() } == true
    }

    /** The main node and every anonymous identity's, those that are open. */
    private fun nodes(): List<ThrenodyNode> =
        listOfNotNull(Threnody.running()) + Threnody.personaIds().mapNotNull(Threnody::personaNode)

    private fun intent(ctx: Context): PendingIntent = PendingIntent.getBroadcast(
        ctx, 0, Intent(ctx, Alarm::class.java),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /** Exact alarms are allowed (always below Android 12). */
    fun exactAllowed(ctx: Context): Boolean {
        if (Build.VERSION.SDK_INT < 31) return true
        return ctx.getSystemService(AlarmManager::class.java)?.canScheduleExactAlarms() == true
    }

    /**
     * Sets the alarm for the next scheduled message of any identity, or
     * cancels it when none is pending. Exact when allowed; otherwise Doze
     * may run it a little late.
     */
    fun arm(ctx: Context) {
        val am = ctx.getSystemService(AlarmManager::class.java) ?: return
        val next = try {
            nodes().mapNotNull { it.nextScheduledMs() }.minOrNull()
        } catch (e: Exception) {
            Threnody.say("! scheduled alarm: ${e.message}")
            return
        }
        val pi = intent(ctx)
        if (next == null) {
            am.cancel(pi)
            return
        }
        val at = next.toLong()
        try {
            if (exactAllowed(ctx)) am.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, at, pi)
            else am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, at, pi)
        } catch (e: SecurityException) {
            am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, at, pi)
        }
    }

    /**
     * Brings the nodes up (which applies the user's settings and opens the
     * gate), sends what's due on each, in parallel, and waits at most
     * [budgetMs] for it to go. Blocking: run it on a worker.
     */
    fun sendDue(ctx: Context, budgetMs: UInt) {
        sending = true
        try {
            Threnody.start(ctx)
            nodes().map { n ->
                Thread {
                    try {
                        val sent = n.runDueScheduled(budgetMs)
                        if (sent > 0u) Threnody.say("* sent $sent scheduled message(s)")
                    } catch (e: Exception) {
                        Threnody.say("! scheduled send: ${e.message}")
                    }
                }.apply { start() }
            }.forEach { it.join() }
            arm(ctx)
        } catch (e: Exception) {
            Threnody.say("! scheduled send: ${e.message}")
        } finally {
            sending = false
        }
    }

    /** What a failure notification says (never the message's text). */
    fun reasonText(r: ScheduleFailure): String = when (r) {
        ScheduleFailure.MISSED -> "missed its time"
        ScheduleFailure.CONTACT -> "the contact was blocked or removed"
        ScheduleFailure.GROUP -> "you're no longer in the group"
        ScheduleFailure.ERROR -> "couldn't send"
        ScheduleFailure.REMOVED -> "this device was removed from your account"
    }

    /**
     * No node runs: start the service in scheduled-send mode (an exact
     * alarm or a boot may start one), or send right here if that's refused.
     */
    private fun wakeToSend(ctx: Context, r: BroadcastReceiver.PendingResult) {
        try {
            ctx.startForegroundService(
                Intent(ctx, ThrenodyService::class.java).setAction(ThrenodyService.ACTION_SCHEDULED)
            )
            r.finish()
        } catch (e: Exception) {
            // An inexact alarm may not start a foreground service: about
            // ten seconds here, then sleep again if push wake-ups are on.
            Threnody.say("! scheduled: no foreground service (${e.message}); sending anyway")
            Thread {
                sendDue(ctx, RECEIVER_BUDGET_MS)
                Push.sleepIfIdle(ctx)
                r.finish()
            }.start()
        }
    }

    /** The alarm went off. */
    class Alarm : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val ctx = context.applicationContext
            val r = goAsync()
            if (Threnody.running() != null) {
                // The node runs (and may be sending already: nothing goes twice).
                Thread {
                    sendDue(ctx, RECEIVER_BUDGET_MS)
                    r.finish()
                }.start()
            } else {
                wakeToSend(ctx, r)
            }
        }
    }

    /**
     * After a reboot or an update (alarms are gone), and when the user
     * allows exact alarms: send what's due and set the alarm again.
     */
    class Boot : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val ctx = context.applicationContext
            if (intent.action == AlarmManager.ACTION_SCHEDULE_EXACT_ALARM_PERMISSION_STATE_CHANGED &&
                Threnody.running() != null
            ) {
                Threading.background { arm(ctx) }
                return
            }
            if (!pendingOnDisk(ctx)) return
            wakeToSend(ctx, goAsync())
        }
    }

    /** What a receiver may spend (it is allowed about ten seconds). */
    private const val RECEIVER_BUDGET_MS = 8_000u
    /** What the service may spend. */
    const val SERVICE_BUDGET_MS = 25_000u
}
