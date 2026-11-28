package org.threnody.app

import android.app.KeyguardManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.media.AudioAttributes
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.speech.tts.TextToSpeech

/**
 * Who's calling, while the phone is unlocked (both off unless turned on):
 *
 * - **Shown.** A ringing call names its caller on screen even where call
 *   notifications otherwise keep quiet about who it is (app lock, private
 *   notifications), as long as the phone is unlocked.
 * - **Said.** A ringing call says the caller's name aloud, a few times,
 *   while the phone is unlocked and its ringer is on.
 *
 * Never on the lock screen: when the phone locks while it rings, the call
 * goes back to saying only that there's a call, and goes quiet; unlocked
 * again, it names them again.
 */
object CallerId {
    private const val PREFS = "calls"
    private const val SHOW = "show_caller_unlocked"
    private const val SAY = "announce_caller"
    /** How often the name is said while it rings, and how many times at most. */
    private const val SAY_EVERY_MS = 7_000L
    private const val SAY_TIMES = 3

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun showWhenUnlocked(ctx: Context) = prefs(ctx).getBoolean(SHOW, false)

    fun setShowWhenUnlocked(ctx: Context, on: Boolean) = prefs(ctx).edit().putBoolean(SHOW, on).apply()

    fun announce(ctx: Context) = prefs(ctx).getBoolean(SAY, false)

    fun setAnnounce(ctx: Context, on: Boolean) = prefs(ctx).edit().putBoolean(SAY, on).apply()

    /** Screen on and past the lock screen (any lock screen, swipe included). */
    fun phoneUnlocked(ctx: Context): Boolean {
        val km = ctx.getSystemService(KeyguardManager::class.java) ?: return false
        val pm = ctx.getSystemService(PowerManager::class.java) ?: return false
        return pm.isInteractive && !km.isKeyguardLocked
    }

    /** Whether to name the caller now, where call notifications otherwise wouldn't. */
    fun nameNow(ctx: Context) = showWhenUnlocked(ctx) && phoneUnlocked(ctx)

    private val main = Handler(Looper.getMainLooper())
    private var app: Context? = null
    private var tts: TextToSpeech? = null
    private var ttsReady = false
    /** What the ringing call says (null when nothing rings), and how often it has. */
    private var words: String? = null
    private var said = 0
    /** Hears the phone lock and unlock while a call rings. */
    private var watcher: BroadcastReceiver? = null
    /** Shows the ringing call again, named or not as the phone now is. */
    private var redraw: (() -> Unit)? = null

    private val sayAgain = object : Runnable {
        override fun run() {
            val ctx = app ?: return
            val w = words ?: return
            if (said >= SAY_TIMES || !maySpeak(ctx)) return
            said++
            speak(ctx, w)
            if (said < SAY_TIMES) main.postDelayed(this, SAY_EVERY_MS)
        }
    }

    private fun maySpeak(ctx: Context) = announce(ctx) && phoneUnlocked(ctx) &&
        ctx.getSystemService(AudioManager::class.java)?.ringerMode == AudioManager.RINGER_MODE_NORMAL

    /**
     * A call rings (or rings on, shown again): `spoken` is what to say, and
     * `again` shows it again when the phone locks or unlocks. From any
     * thread: this and [stop] run on the main thread in the order called.
     */
    fun ringing(ctx: Context, spoken: String, again: () -> Unit) {
        val a = ctx.applicationContext
        main.post { start(a, spoken, again) }
    }

    private fun start(a: Context, spoken: String, again: () -> Unit) {
        app = a
        redraw = again
        if (watcher == null) {
            val r = object : BroadcastReceiver() {
                override fun onReceive(c: Context, i: Intent) {
                    if (words == null) return
                    if (i.action == Intent.ACTION_SCREEN_OFF) {
                        main.removeCallbacks(sayAgain)
                        tts?.stop()
                    }
                    redraw?.invoke()
                }
            }
            val filter = IntentFilter().apply {
                addAction(Intent.ACTION_USER_PRESENT)
                addAction(Intent.ACTION_SCREEN_OFF)
            }
            try {
                if (Build.VERSION.SDK_INT >= 33) a.registerReceiver(r, filter, Context.RECEIVER_NOT_EXPORTED)
                else a.registerReceiver(r, filter)
                watcher = r
            } catch (e: Exception) {
                Threnody.say("! caller id: ${e.javaClass.simpleName}")
            }
        }
        val first = words == null
        words = spoken
        // Said as it starts ringing, or when the phone is unlocked meanwhile.
        if (first || said == 0) {
            main.removeCallbacks(sayAgain)
            if (maySpeak(a)) main.post(sayAgain)
        }
    }

    /** The call stopped ringing: quiet, and no more watching the lock. */
    fun stop() {
        main.post { quiet() }
    }

    private fun quiet() {
        words = null
        said = 0
        redraw = null
        main.removeCallbacks(sayAgain)
        watcher?.let { r -> runCatching { app?.unregisterReceiver(r) } }
        watcher = null
        tts?.stop()
        tts?.shutdown()
        tts = null
        ttsReady = false
    }

    private fun speak(ctx: Context, text: String) {
        val t = tts
        if (t == null) {
            tts = TextToSpeech(ctx) { status ->
                main.post {
                    if (status == TextToSpeech.SUCCESS && tts != null) {
                        ttsReady = true
                        // Still ringing, still unlocked: say it now.
                        words?.let { w -> if (maySpeak(ctx)) utter(w) }
                    } else {
                        tts?.shutdown()
                        tts = null
                    }
                }
            }
            return
        }
        if (ttsReady) utter(text)
    }

    private fun utter(text: String) {
        val t = tts ?: return
        t.setAudioAttributes(
            AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                .build()
        )
        t.speak(text, TextToSpeech.QUEUE_FLUSH, null, "threnody-caller")
    }
}
