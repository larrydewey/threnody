package org.threnody.app

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.util.Base64
import java.util.concurrent.TimeUnit
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Waking with UnifiedPush instead of a permanent connection (Appendix R).
 * Off unless the user turns it on: the push server then sees when this
 * phone is woken and the IP addresses of the contacts that wake it, and
 * the phone no longer sends cover traffic while it sleeps.
 *
 * When on and a distributor gave an endpoint, the app keeps no foreground
 * service in the background: a minute after it leaves the screen it shuts
 * its nodes down and ends its process. A wake-up starts [ThrenodyService]
 * in wake mode, which syncs and sleeps again.
 */
object Push {
    private const val PREFS = "push"
    private const val ENABLED = "enabled"
    private const val URL = "url"
    private const val KEY = "p256dh"
    private const val AUTH = "auth"
    private const val SINCE = "registered_at"
    /** Our one registration with the distributor. */
    const val INSTANCE = "default"
    /** A fresh endpoint this often, so an old one stops identifying us. */
    private val ROTATE_MS = TimeUnit.DAYS.toMillis(30)
    /** Grace in the background before sleeping, for a quick app switch. */
    private const val SLEEP_AFTER_S = 60L
    /** Longest a wake-up sync runs. */
    const val WAKE_MS = 60_000u

    /** A wake-up sync is running: don't sleep under it. */
    @Volatile var waking = false

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The user turned it on. */
    fun enabled(ctx: Context) = prefs(ctx).getBoolean(ENABLED, false)

    /** On, with an endpoint: the app may sleep in the background. */
    fun active(ctx: Context) = enabled(ctx) && prefs(ctx).getString(URL, null) != null

    /** At least one UnifiedPush distributor is installed. */
    fun available(ctx: Context) = UnifiedPush.getDistributors(ctx).isNotEmpty()

    /**
     * Turns it on: picks the default distributor (asking the user if
     * there are several) and registers. [done] gets false when there is
     * no distributor.
     */
    fun enable(a: Activity, done: (Boolean) -> Unit) {
        UnifiedPush.tryUseCurrentOrDefaultDistributor(a) { ok ->
            if (ok) {
                prefs(a).edit().putBoolean(ENABLED, true).apply()
                UnifiedPush.register(a, INSTANCE, "Threnody wake-ups")
            }
            done(ok)
        }
    }

    /** Turns it off: unregisters, withdraws the endpoint, and stays connected again. */
    fun disable(ctx: Context) {
        prefs(ctx).edit().clear().apply()
        try {
            UnifiedPush.unregister(ctx, INSTANCE)
        } catch (e: Exception) {
            Threnody.say("! push unregister: ${e.message}")
        }
        Threading.background { Threnody.running()?.let { apply(ctx, it) } }
        ThrenodyService.start(ctx)
    }

    /**
     * Hands the node our endpoint, or withdraws it; rotates an old one.
     * Main identity only: anonymous identities are never woken by push.
     */
    fun apply(ctx: Context, node: ThrenodyNode) {
        val p = prefs(ctx)
        val url = p.getString(URL, null)
        if (enabled(ctx) && url != null && System.currentTimeMillis() - p.getLong(SINCE, 0) > ROTATE_MS) {
            Threnody.say("* push: rotating the endpoint")
            p.edit().remove(URL).remove(KEY).remove(AUTH).apply()
            try {
                UnifiedPush.unregister(ctx, INSTANCE)
                UnifiedPush.register(ctx, INSTANCE, "Threnody wake-ups")
            } catch (e: Exception) {
                Threnody.say("! push rotate: ${e.message}")
            }
        }
        val endpoint = if (active(ctx)) {
            uniffi.threnody_ffi.PushEndpoint(
                p.getString(URL, null)!!,
                p.getString(KEY, null)?.let(::b64),
                p.getString(AUTH, null)?.let(::b64),
            )
        } else null
        try {
            node.setPushEndpoint(endpoint)
        } catch (e: Exception) {
            Threnody.say("! push endpoint: ${e.message}")
        }
    }

    private fun b64(s: String): ByteArray = Base64.decode(s, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)

    internal fun onEndpoint(ctx: Context, e: PushEndpoint) {
        if (!enabled(ctx)) return
        prefs(ctx).edit()
            .putString(URL, e.url)
            .putString(KEY, e.pubKeySet?.pubKey)
            .putString(AUTH, e.pubKeySet?.auth)
            .putLong(SINCE, System.currentTimeMillis())
            .apply()
        Threnody.say("* push: new endpoint${if (e.pubKeySet == null) " (no Web Push keys)" else ""}")
        Threnody.running()?.let { n -> Threading.background { apply(ctx, n) } }
    }

    internal fun onGone(ctx: Context, why: String) {
        prefs(ctx).edit().remove(URL).remove(KEY).remove(AUTH).apply()
        Threnody.say("* push: $why; staying connected instead")
        Threnody.running()?.let { n -> Threading.background { apply(ctx, n) } }
    }

    /** A wake-up: sync in the foreground service, or right here if that's refused. */
    internal fun onWake(ctx: Context) {
        if (!active(ctx)) return
        waking = true
        try {
            ctx.startForegroundService(Intent(ctx, ThrenodyService::class.java).setAction(ThrenodyService.ACTION_WAKE))
        } catch (e: Exception) {
            Threnody.say("! push: no foreground service (${e.message}); syncing anyway")
            Thread { sync(ctx); sleepIfIdle(ctx) }.start()
        }
    }

    /** Brings every identity's node up and lets contacts deliver what they held. */
    fun sync(ctx: Context) {
        waking = true
        try {
            val main = Threnody.start(ctx)
            if (Threnody.visible > 0) {
                main.reconnect()
                return
            }
            val nodes = listOf(main) + Threnody.personaIds().mapNotNull(Threnody::personaNode)
            val threads = nodes.map { n ->
                Thread {
                    val r = n.wakeSync(WAKE_MS)
                    Threnody.say("* woken: ${r.connected} of ${r.contacts} contacts connected")
                }.apply { start() }
            }
            threads.forEach { it.join() }
            // Let the event pump post the notifications.
            Thread.sleep(1500)
        } catch (e: Exception) {
            Threnody.say("! push sync: ${e.message}")
        } finally {
            waking = false
        }
    }

    /** The app left the screen: sleep after a grace period if still unseen. */
    fun backgrounded(ctx: Context) {
        if (!active(ctx)) return
        Threading.scheduleOnce(SLEEP_AFTER_S, TimeUnit.SECONDS) { sleepIfIdle(ctx) }
    }

    /**
     * Sleeps if nothing needs the node: shuts every node down and ends the
     * process, so nothing runs until the next wake-up or the user opens
     * the app. Never during a call, a wake-up, or with WireGuard tunnels on.
     */
    @Synchronized
    fun sleepIfIdle(ctx: Context) {
        if (!active(ctx) || Threnody.visible > 0 || waking || Calls.current != null || Privacy.tunnels(ctx)) return
        Threnody.say("* push: sleeping until woken")
        ctx.stopService(Intent(ctx, ThrenodyService::class.java))
        for (id in Threnody.personaIds()) Threnody.personaNode(id)?.shutdown()
        Threnody.running()?.shutdown()
        Runtime.getRuntime().exit(0)
    }
}

/** Receives endpoints and wake-ups from the UnifiedPush distributor. */
class ThrenodyPushService : PushService() {
    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        if (instance == Push.INSTANCE) Push.onEndpoint(applicationContext, endpoint)
    }

    override fun onMessage(message: PushMessage, instance: String) {
        // The content is random: a wake-up says only "come and look".
        if (instance == Push.INSTANCE) Push.onWake(applicationContext)
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        if (instance == Push.INSTANCE) Push.onGone(applicationContext, "registration failed ($reason)")
    }

    override fun onUnregistered(instance: String) {
        if (instance == Push.INSTANCE) Push.onGone(applicationContext, "unregistered by the distributor")
    }
}
