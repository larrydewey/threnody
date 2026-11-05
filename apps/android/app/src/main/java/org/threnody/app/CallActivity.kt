package org.threnody.app

import android.app.Activity
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.SurfaceTexture
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Gravity
import android.view.TextureView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import java.nio.ByteBuffer

/**
 * The call screen: who, how the call is going, the video (theirs filling
 * the screen, ours in the corner), and the controls: answer (voice or
 * video) or decline while it rings here; mute, camera, speaker and hang up
 * after. Shown over the lock screen for an incoming call. The camera runs
 * only while this screen is open. A group call shows a tile for each other
 * participant (their video, or their name) instead of one full picture.
 */
class CallActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var who: TextView
    private lateinit var ringingRow: LinearLayout
    private lateinit var activeRow: LinearLayout
    private lateinit var answerVideo: ImageButton
    private lateinit var mute: ImageButton
    private lateinit var camera: ImageButton
    private lateinit var speaker: ImageButton
    private lateinit var flip: ImageButton
    private lateinit var remote: ImageView
    /** A group call's participants, a tile each. */
    private lateinit var tiles: android.widget.GridLayout
    private val tileViews = mutableMapOf<String, ImageView>()
    private lateinit var local: TextureView
    private var unlisten: (() -> Unit)? = null
    private var unlistenReactions: (() -> Unit)? = null
    /** Emoji floating up over the video, ours and the peer's (draws only, takes no touches). */
    private lateinit var floats: FrameLayout
    /** The emoji button, and the row of emoji it opens. */
    private lateinit var reactions: LinearLayout
    private lateinit var picks: android.widget.HorizontalScrollView
    private val ticker = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            show()
            ticker.postDelayed(this, 1000)
        }
    }
    private var feed: CallCamera? = null
    /** Which camera `feed` is. */
    private var feedFront = true
    /** Which drawing thread is current: each start makes a new one, and older ones stop. */
    private val watching = java.util.concurrent.atomic.AtomicInteger(0)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        // An incoming call shows over the lock screen and wakes it.
        setShowWhenLocked(true)
        setTurnScreenOn(true)

        who = label("", Design.typeTitle * 1.4f, R.color.text, Design.weightBold).apply {
            gravity = Gravity.CENTER
            setShadowLayer(4f, 0f, 1f, Color.BLACK)
        }
        status = label("", Design.typeSubtitle, R.color.muted).apply {
            gravity = Gravity.CENTER
            setShadowLayer(4f, 0f, 1f, Color.BLACK)
        }

        fun round(res: Int, label: String, bg: Int, fg: Int, onClick: () -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(fg)
            contentDescription = label
            tooltipText = label
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(bg)
            }
            setOnClickListener { v ->
                Design.mediumHaptic(v)
                onClick()
            }
        }
        val red = getColor(android.R.color.holo_red_dark)
        val green = getColor(android.R.color.holo_green_dark)
        val white = getColor(android.R.color.white)
        val big = dp(72)
        fun row(vararg views: View) = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER
            for (v in views) addView(v, LinearLayout.LayoutParams(big, big).apply {
                marginStart = dp(Design.sm)
                marginEnd = dp(Design.sm)
            })
        }
        answerVideo = round(R.drawable.ic_videocam, "Answer with video", green, white) { Calls.answer(this, video = true) }
        ringingRow = row(
            round(R.drawable.ic_call_end, "Decline", red, white) { Calls.hangUp(this) },
            round(R.drawable.ic_call, "Answer", green, white) { Calls.answer(this) },
            answerVideo,
        )
        val idle = getColor(R.color.surface_container)
        val text = getColor(R.color.text)
        mute = round(R.drawable.ic_mic_off, "Mute", idle, text) {
            Calls.current?.let { Calls.setMuted(!it.muted) }
        }
        camera = round(R.drawable.ic_videocam, "Camera", idle, text) {
            Calls.current?.let { Calls.setVideo(this, !it.video) }
        }
        speaker = round(R.drawable.ic_speaker, "Speaker", idle, text) {
            Calls.current?.let { Calls.setSpeaker(this, !it.speaker) }
        }
        // Over our own picture: front or back camera, when there are both.
        flip = ImageButton(this).apply {
            setImageResource(R.drawable.ic_cameraswitch)
            imageTintList = android.content.res.ColorStateList.valueOf(white)
            contentDescription = "Switch camera"
            tooltipText = "Switch camera"
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(Color.argb(140, 0, 0, 0))
            }
            visibility = View.GONE
            setOnClickListener { v ->
                Design.mediumHaptic(v)
                Calls.switchCamera()
            }
        }
        activeRow = row(
            mute,
            camera,
            round(R.drawable.ic_call_end, "Hang up", red, white) { Calls.hangUp(this) },
            speaker,
        )

        remote = ImageView(this).apply {
            scaleType = ImageView.ScaleType.FIT_CENTER
            setBackgroundColor(Color.BLACK)
            visibility = View.GONE
        }
        // The camera's frames are landscape, turned upright: a 3:4 window.
        local = TextureView(this).apply {
            visibility = View.GONE
            surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                override fun onSurfaceTextureAvailable(st: SurfaceTexture, w: Int, h: Int) = cameraIfWanted()
                override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, w: Int, h: Int) {}
                override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
                    stopCamera()
                    return true
                }
                override fun onSurfaceTextureUpdated(st: SurfaceTexture) {}
            }
        }

        tiles = android.widget.GridLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            visibility = View.GONE
        }

        floats = FrameLayout(this)
        val strip = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        picks = android.widget.HorizontalScrollView(this).apply {
            isHorizontalScrollBarEnabled = false
            background = rounded(Color.argb(140, 0, 0, 0), dp(26).toFloat())
            setPadding(dp(6), dp(2), dp(6), dp(2))
            visibility = View.GONE
            addView(strip)
        }
        val emojiButton = ImageButton(this).apply {
            setImageResource(R.drawable.ic_emoji)
            imageTintList = android.content.res.ColorStateList.valueOf(white)
            contentDescription = "Send a reaction"
            tooltipText = "Send a reaction"
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(Color.argb(140, 0, 0, 0))
            }
            setOnClickListener { v ->
                Design.lightHaptic(v)
                if (picks.visibility == View.VISIBLE) {
                    picks.visibility = View.GONE
                } else {
                    fillPicks(strip)
                    picks.visibility = View.VISIBLE
                }
            }
        }
        reactions = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL or Gravity.END
            visibility = View.GONE
            addView(picks, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginEnd = dp(Design.sm) })
            addView(emojiButton, LinearLayout.LayoutParams(dp(52), dp(52)))
        }

        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(dp(Design.xl), dp(Design.xxl * 2), dp(Design.xl), dp(Design.xxl * 2))
            addView(who, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(Design.sm) })
            addView(View(this@CallActivity), LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            addView(reactions, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(Design.lg) })
            addView(ringingRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(activeRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        val frame = FrameLayout(this).apply {
            setBackgroundColor(color(R.color.background))
            addView(remote, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(tiles, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(floats, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(content, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(local, FrameLayout.LayoutParams(dp(108), dp(144), Gravity.TOP or Gravity.END).apply {
                topMargin = dp(Design.xxl * 3)
                marginEnd = dp(Design.lg)
            })
            addView(flip, FrameLayout.LayoutParams(dp(40), dp(40), Gravity.TOP or Gravity.END).apply {
                topMargin = dp(Design.xxl * 3) + dp(144) - dp(48)
                marginEnd = dp(Design.lg) + dp(8)
            })
        }
        setContentView(frame)
        fitSystemBars(frame, content, content)
        // Answer, from the ringing notification.
        if (intent.action == ANSWER) Calls.answer(this)
    }

    override fun onNewIntent(intent: android.content.Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.action == ANSWER) Calls.answer(this)
    }

    override fun onStart() {
        super.onStart()
        unlisten = Calls.listen { runOnUiThread { show() } }
        unlistenReactions = Calls.listenReactions { e -> runOnUiThread { float(e) } }
        ticker.post(tick)
        watchVideo()
        Calls.pauseVideo(false)
        show()
    }

    override fun onStop() {
        unlisten?.invoke()
        unlistenReactions?.invoke()
        floats.removeAllViews()
        ticker.removeCallbacks(tick)
        watching.incrementAndGet()
        stopCamera()
        Calls.pauseVideo(true)
        super.onStop()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        Calls.permissionResult(code, results)
    }

    /** Runs the camera while the call wants our video and the preview exists. */
    private fun cameraIfWanted() {
        val c = Calls.current
        if (c == null || !c.video || c.state == "incoming") return stopCamera()
        // Switched: close this camera and open the other.
        if (feed != null && feedFront != c.frontCamera) stopCamera()
        if (feed != null) return
        val st = local.surfaceTexture ?: return
        feedFront = c.frontCamera
        feed = CallCamera(applicationContext, c.node, c.frontCamera).also { it.start(st) }
    }

    private fun stopCamera() {
        feed?.stop()
        feed = null
    }

    /** Pulls the peer's newest frames while the screen shows. */
    private fun watchVideo() {
        val me = watching.incrementAndGet()
        Thread {
            var bitmap: Bitmap? = null
            // For the diagnostics log: how many frames, never what's in them.
            var (count, since) = 0 to SystemClock.elapsedRealtime()
            while (watching.get() == me) {
                if (SystemClock.elapsedRealtime() - since >= 5000) {
                    if (count > 0) Threnody.say("* video: $count frames in 5 s")
                    count = 0
                    since = SystemClock.elapsedRealtime()
                }
                val c = Calls.current ?: break
                // A group call's frames come from each participant in turn.
                val (f, from) = if (c.group != null) {
                    val g = try { c.node.nextGroupVideoFrame(250u) } catch (_: Exception) { null } ?: continue
                    Triple(g.width, g.height, g.rotation) to (g.rgba to g.device)
                } else {
                    val v = try { c.node.nextVideoFrame(250u) } catch (_: Exception) { null } ?: continue
                    Triple(v.width, v.height, v.rotation) to (v.rgba to null)
                }
                val (width, height, rotation) = f
                val (rgba, device) = from
                val (w, h) = width.toInt() to height.toInt()
                val b = bitmap?.takeIf { it.width == w && it.height == h }
                    ?: Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).also { bitmap = it }
                // ARGB_8888 is R, G, B, A in memory, as the frame is.
                b.copyPixelsFromBuffer(ByteBuffer.wrap(rgba))
                val shown = if (rotation == 0u) b.copy(Bitmap.Config.ARGB_8888, false)
                else Bitmap.createBitmap(b, 0, 0, w, h, Matrix().apply { postRotate(rotation.toFloat()) }, true)
                count++
                runOnUiThread {
                    if (device == null) remote.setImageBitmap(shown) else tileViews[device]?.setImageBitmap(shown)
                }
            }
        }.apply { name = "call-video"; isDaemon = true }.start()
    }

    private fun show() {
        val c = Calls.current
        if (c == null) {
            // Over: say how, briefly, then go.
            status.text = Calls.ended ?: "Call ended"
            ringingRow.visibility = View.GONE
            activeRow.visibility = View.GONE
            remote.visibility = View.GONE
            local.visibility = View.GONE
            stopCamera()
            ticker.removeCallbacks(tick)
            ticker.postDelayed({ finish() }, 1200)
            return
        }
        who.text = c.title
        val group = c.group != null
        status.text = when (c.state) {
            "reaching" -> "Reaching ${c.title}…"
            "calling" -> if (group) "Waiting for others to join…" else if (c.video) "Video calling…" else "Calling…"
            "ringing" -> "Ringing…"
            "incoming" -> if (group) {
                "Group call from ${c.members.keys.firstOrNull()?.let { Threnody.nameOf(c.node, it) } ?: "a member"}"
            } else if (c.peerVideo) "Incoming video call" else "Incoming voice call"
            "connecting" -> "Connecting…"
            "interrupted" -> "Reconnecting…"
            else -> duration(SystemClock.elapsedRealtime() - c.since) +
                if (group) " · ${c.members.size + 1} people" else ""
        }
        val ringing = c.state == "incoming"
        ringingRow.visibility = if (ringing) View.VISIBLE else View.GONE
        // Joining a group call, either way is fine.
        answerVideo.visibility = if (c.peerVideo || group) View.VISIBLE else View.GONE
        answerVideo.contentDescription = if (group) "Join with video" else "Answer with video"
        activeRow.visibility = if (ringing) View.GONE else View.VISIBLE
        val talking = c.state == "connected" || c.state == "interrupted"
        // Reactions are for calls with one contact.
        reactions.visibility = if (talking && !group) View.VISIBLE else View.GONE
        if (!talking || group) picks.visibility = View.GONE
        remote.visibility = if (c.peerVideo && !ringing && !group) View.VISIBLE else View.GONE
        showTiles(c, group && !ringing)
        local.visibility = if (c.video && !ringing) View.VISIBLE else View.GONE
        flip.visibility = if (c.video && !ringing && CallCamera.canSwitch(this)) View.VISIBLE else View.GONE
        mute.alpha = if (c.muted) 1f else 0.6f
        mute.contentDescription = if (c.muted) "Unmute" else "Mute"
        camera.alpha = if (c.video) 1f else 0.6f
        camera.contentDescription = if (c.video) "Turn camera off" else "Turn camera on"
        speaker.alpha = if (c.speaker) 1f else 0.6f
        cameraIfWanted()
    }

    /**
     * A group call's participants as a grid: their video when they send it,
     * else their name. Rebuilt only when who's there changes.
     */
    private fun showTiles(c: Calls.Call, visible: Boolean) {
        tiles.visibility = if (visible && c.members.isNotEmpty()) View.VISIBLE else View.GONE
        if (!visible) return
        val who = c.members.keys.sorted()
        if (who != tileViews.keys.sorted()) {
            tiles.removeAllViews()
            tileViews.clear()
            tiles.columnCount = when {
                who.size <= 1 -> 1
                who.size <= 4 -> 2
                else -> 3
            }
            for (d in who) {
                val picture = ImageView(this).apply { scaleType = ImageView.ScaleType.CENTER_CROP }
                val name = label(Threnody.nameOf(c.node, d), Design.typeSubtitle, R.color.text).apply {
                    setShadowLayer(4f, 0f, 1f, Color.BLACK)
                }
                val tile = FrameLayout(this).apply {
                    setBackgroundColor(Color.rgb(24, 24, 24))
                    addView(picture, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
                    addView(name, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.BOTTOM or Gravity.START).apply {
                        setMargins(dp(Design.sm), dp(Design.sm), dp(Design.sm), dp(Design.sm))
                    })
                }
                tiles.addView(tile, android.widget.GridLayout.LayoutParams(
                    android.widget.GridLayout.spec(android.widget.GridLayout.UNDEFINED, 1f),
                    android.widget.GridLayout.spec(android.widget.GridLayout.UNDEFINED, 1f),
                ).apply {
                    width = 0
                    height = 0
                    setMargins(1, 1, 1, 1)
                })
                tileViews[d] = picture
            }
        }
        // Without video, a tile shows just the name.
        for ((d, picture) in tileViews) {
            if (c.members[d] != true) picture.setImageDrawable(null)
        }
    }

    /** The row of emoji to send: recent ones first, then the usual; ＋ opens them all. */
    private fun fillPicks(strip: LinearLayout) {
        strip.removeAllViews()
        fun pick(text: String, desc: String, onClick: () -> Unit) = strip.addView(TextView(this).apply {
            this.text = text
            textSize = 28f
            gravity = Gravity.CENTER
            contentDescription = desc
            setTextColor(Color.WHITE)
            setOnClickListener { v ->
                Design.lightHaptic(v)
                onClick()
            }
        }, LinearLayout.LayoutParams(dp(48), dp(48)))
        for (e in (EmojiPicker.recent(this) + QUICK).distinct().take(QUICK.size)) {
            pick(e, "Send $e") { Calls.react(this, e) }
        }
        pick("＋", "All emoji") {
            EmojiPicker(this, "Reaction", stay = true) { e -> Calls.react(this, e) }.show()
        }
    }

    /**
     * Floats `emoji` up from the bottom of the screen, swaying a little,
     * and fades it out. A flood is capped: the oldest give way.
     */
    private fun float(emoji: String) {
        if (floats.width == 0) return
        while (floats.childCount >= MAX_FLOATING) floats.removeViewAt(0)
        val v = TextView(this).apply {
            text = emoji
            textSize = 40f
            gravity = Gravity.CENTER
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        val rnd = java.util.Random()
        val x = floats.width * (0.15f + 0.6f * rnd.nextFloat())
        floats.addView(v, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.TOP or Gravity.START))
        val (from, to) = floats.height * 0.8f to floats.height * 0.15f
        val sway = dp(16) * (if (rnd.nextBoolean()) 1f else -1f)
        android.animation.ValueAnimator.ofFloat(0f, 1f).apply {
            duration = FLOAT_MS
            interpolator = android.view.animation.DecelerateInterpolator()
            addUpdateListener {
                val t = it.animatedValue as Float
                v.translationX = x + sway * kotlin.math.sin(t * 3 * Math.PI).toFloat()
                v.translationY = from + (to - from) * t
                val grow = 0.6f + 0.6f * kotlin.math.min(1f, t * 4)
                v.scaleX = grow
                v.scaleY = grow
                // Fades over the last third of the way up.
                v.alpha = if (t < 2f / 3) 1f else (1 - t) * 3
            }
            addListener(object : android.animation.AnimatorListenerAdapter() {
                override fun onAnimationEnd(a: android.animation.Animator) {
                    floats.removeView(v)
                }
            })
            start()
        }
    }

    private fun duration(ms: Long): String {
        val s = ms / 1000
        return if (s >= 3600) "%d:%02d:%02d".format(s / 3600, s / 60 % 60, s % 60) else "%d:%02d".format(s / 60, s % 60)
    }

    companion object {
        const val ANSWER = "org.threnody.app.ANSWER"
        private const val FLOAT_MS = 2800L
        private const val MAX_FLOATING = 24
        private val QUICK = listOf("❤️", "😂", "👍", "😮", "😢", "🎉", "🔥", "😘")
    }

    // The call carries on without the screen; Back just leaves it.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        moveTaskToBack(false)
    }
}
