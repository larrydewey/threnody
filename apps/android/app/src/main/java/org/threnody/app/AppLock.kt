package org.threnody.app

import android.app.Activity
import android.app.Application
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.Window
import java.util.WeakHashMap
import uniffi.threnody_ffi.AppLockClock
import uniffi.threnody_ffi.LockSettings
import uniffi.threnody_ffi.appLock
import uniffi.threnody_ffi.appLockEnabled

/**
 * App lock (Appendix C, *App lock*): the gate in front of every screen.
 *
 * The decisions are in Rust ([AppLockClock], the lock file); this only
 * feeds the clock what the activities do and shows [LockActivity] when
 * it says so. App lock is on exactly while its file exists, which needs
 * no Keystore secret to check, so a cold start (and a notification
 * posted at boot) fails closed before the settings have been read.
 *
 * The node keeps running behind the lock: it is a gate on the screen,
 * not encryption at rest.
 */
object AppLock {
    /** Screens shown while locked: the lock screen, and a ringing call. */
    private val exempt = setOf(LockActivity::class.java, CallActivity::class.java)
    private const val TICK_MS = 15_000L

    @Volatile private var clock = AppLockClock(false, true, 0u, 300u, 0u)
    /** The settings once read (null until then, or when unreadable). */
    @Volatile var settings: LockSettings? = null
        private set
    /** The lock file exists but can't be read: stay locked, offer only erasing. */
    @Volatile var unreadable = false
        private set

    private var started = 0
    private var expecting = false
    private var resumed: Activity? = null
    private var lastTouch = 0L
    private val live = WeakHashMap<Activity, Unit>()
    private val main = Handler(Looper.getMainLooper())

    fun now(): ULong = SystemClock.elapsedRealtime().toULong()

    private fun home(ctx: Context) = Threnody.home(ctx)

    /** Whether app lock is on (the lock file exists). Cheap and synchronous. */
    fun enabled(ctx: Context): Boolean = appLockEnabled(home(ctx))

    /** Whether the gate is closed now. */
    fun locked(): Boolean = clock.isLocked()

    /** From Application.onCreate: before any activity starts. */
    fun init(app: Application) {
        val on = enabled(app)
        clock = AppLockClock(on, true, 0u, 300u, now())
        // No lock file, no Keystore: on a fresh install that would create
        // a key while `Threnody.open` rotates it.
        if (on) Threading.background { reload(app) }
        app.registerActivityLifecycleCallbacks(callbacks)
    }

    /** Reads the settings (off the main thread) and applies their times. */
    fun reload(ctx: Context): LockSettings? = try {
        val s = appLock(home(ctx), KeyVault.secret(ctx))
        settings = s
        unreadable = false
        clock.setPolicy(s.biometric, s.awaySecs, s.idleSecs)
        s
    } catch (e: Exception) {
        // Locked closed: the lock screen offers only erasing.
        Threnody.say("! app lock settings: ${e.message}")
        settings = null
        unreadable = enabled(ctx)
        null
    }

    /** The settings changed in [AppLockUi]. */
    fun changed(s: LockSettings) {
        settings = s
        unreadable = false
        clock.setPolicy(s.biometric, s.awaySecs, s.idleSecs)
        // Turned off: nothing stays locked. Turned on or changed in front:
        // the idle time counts from now.
        if (!s.enabled || !clock.isLocked()) clock.unlocked(now())
        // Every screen, not just the one that changed it: recents must not
        // keep a snapshot of the activities under it either.
        main.post { for (a in live.keys) Privacy.apply(a) }
    }

    /**
     * The app is about to open another app's picker or camera: a few
     * minutes' grace. Cleared as soon as one of ours is in front again,
     * so it never carries over to leaving the app later.
     */
    fun expectReturn() {
        expecting = true
    }

    /** Locks at once ("Lock now"). */
    fun lockNow(a: Activity) {
        clock.lockNow()
        showLock(a)
    }

    /** The lock screen let the user in. */
    fun onUnlocked() {
        clock.unlocked(now())
        main.post { for (a in live.keys) a.window?.decorView?.visibility = View.VISIBLE }
    }

    /** After an erase: as on a fresh install, nothing is locked. */
    fun reset() {
        clock = AppLockClock(false, true, 0u, 300u, now())
        settings = null
        unreadable = false
    }

    private fun isExempt(a: Activity) = a.javaClass in exempt

    private fun hide(a: Activity) {
        a.window?.decorView?.visibility = View.INVISIBLE
    }

    private fun showLock(from: Activity) {
        if (isExempt(from)) return
        hide(from)
        from.startActivity(
            Intent(from, LockActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NO_ANIMATION or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        )
        @Suppress("DEPRECATION")
        from.overridePendingTransition(0, 0)
    }

    private fun touched() {
        val t = SystemClock.elapsedRealtime()
        if (t - lastTouch < 1_000) return
        lastTouch = t
        clock.activity(t.toULong())
    }

    private val tick = object : Runnable {
        override fun run() {
            if (started == 0) return
            val a = resumed
            if (a != null && enabled(a) && clock.tick(now())) showLock(a)
            main.postDelayed(this, TICK_MS)
        }
    }

    /** Watches input to postpone the idle lock. */
    private class Watch(private val inner: Window.Callback) : Window.Callback by inner {
        override fun dispatchTouchEvent(event: MotionEvent?): Boolean {
            touched()
            return inner.dispatchTouchEvent(event)
        }

        override fun dispatchKeyEvent(event: KeyEvent?): Boolean {
            touched()
            return inner.dispatchKeyEvent(event)
        }
    }

    private val callbacks = object : Application.ActivityLifecycleCallbacks {
        override fun onActivityCreated(a: Activity, saved: Bundle?) {
            live[a] = Unit
            a.window?.let { w -> w.callback?.let { if (it !is Watch) w.callback = Watch(it) } }
            Privacy.apply(a)
        }

        override fun onActivityStarted(a: Activity) {
            started++
            // App lock off: the clock isn't consulted (the tick checks again).
            val on = enabled(a)
            if (started == 1) {
                if (on) clock.returned(now())
                main.removeCallbacks(tick)
                main.postDelayed(tick, TICK_MS)
            }
            // Before its first frame: an activity made while locked (a
            // notification tap, a link) must not show content.
            if (on && clock.isLocked() && !isExempt(a)) hide(a)
        }

        override fun onActivityResumed(a: Activity) {
            resumed = a
            // The picker came back, or was one of our own screens.
            expecting = false
            if (isExempt(a)) return
            if (enabled(a) && clock.tick(now())) showLock(a)
            else a.window?.decorView?.visibility = View.VISIBLE
        }

        override fun onActivityPaused(a: Activity) {
            if (resumed === a) resumed = null
        }

        override fun onActivityStopped(a: Activity) {
            started--
            if (started > 0) return
            main.removeCallbacks(tick)
            if (!enabled(a)) {
                expecting = false
                return
            }
            if (expecting) clock.leftExpectingReturn(now()) else clock.left(now())
            expecting = false
            // Recents shows nothing either (FLAG_SECURE covers the rest).
            if (clock.isLocked()) for (x in live.keys) if (!isExempt(x)) hide(x)
        }

        override fun onActivitySaveInstanceState(a: Activity, out: Bundle) {}

        override fun onActivityDestroyed(a: Activity) {
            live.remove(a)
        }
    }
}
