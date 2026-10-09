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
 * only while this screen is open.
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
    private lateinit var remote: ImageView
    private lateinit var local: TextureView
    private var unlisten: (() -> Unit)? = null
    private val ticker = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            show()
            ticker.postDelayed(this, 1000)
        }
    }
    private var feed: CallCamera? = null
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

        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(dp(Design.xl), dp(Design.xxl * 2), dp(Design.xl), dp(Design.xxl * 2))
            addView(who, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(Design.sm) })
            addView(View(this@CallActivity), LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            addView(ringingRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(activeRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        val frame = FrameLayout(this).apply {
            setBackgroundColor(color(R.color.background))
            addView(remote, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(content, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(local, FrameLayout.LayoutParams(dp(108), dp(144), Gravity.TOP or Gravity.END).apply {
                topMargin = dp(Design.xxl * 3)
                marginEnd = dp(Design.lg)
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
        ticker.post(tick)
        watchVideo()
        show()
    }

    override fun onStop() {
        unlisten?.invoke()
        ticker.removeCallbacks(tick)
        watching.incrementAndGet()
        stopCamera()
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
        if (feed != null) return
        val st = local.surfaceTexture ?: return
        feed = CallCamera(applicationContext, c.node).also { it.start(st) }
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
                val f = try { c.node.nextVideoFrame(250u) } catch (_: Exception) { null } ?: continue
                val (w, h) = f.width.toInt() to f.height.toInt()
                val b = bitmap?.takeIf { it.width == w && it.height == h }
                    ?: Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).also { bitmap = it }
                // ARGB_8888 is R, G, B, A in memory, as the frame is.
                b.copyPixelsFromBuffer(ByteBuffer.wrap(f.rgba))
                val shown = if (f.rotation == 0u) b.copy(Bitmap.Config.ARGB_8888, false)
                else Bitmap.createBitmap(b, 0, 0, w, h, Matrix().apply { postRotate(f.rotation.toFloat()) }, true)
                count++
                runOnUiThread { remote.setImageBitmap(shown) }
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
        status.text = when (c.state) {
            "calling" -> if (c.video) "Video calling…" else "Calling…"
            "ringing" -> "Ringing…"
            "incoming" -> if (c.peerVideo) "Incoming video call" else "Incoming voice call"
            "connecting" -> "Connecting…"
            "interrupted" -> "Reconnecting…"
            else -> duration(SystemClock.elapsedRealtime() - c.since)
        }
        val ringing = c.state == "incoming"
        ringingRow.visibility = if (ringing) View.VISIBLE else View.GONE
        answerVideo.visibility = if (c.peerVideo) View.VISIBLE else View.GONE
        activeRow.visibility = if (ringing) View.GONE else View.VISIBLE
        remote.visibility = if (c.peerVideo && !ringing) View.VISIBLE else View.GONE
        local.visibility = if (c.video && !ringing) View.VISIBLE else View.GONE
        mute.alpha = if (c.muted) 1f else 0.6f
        mute.contentDescription = if (c.muted) "Unmute" else "Mute"
        camera.alpha = if (c.video) 1f else 0.6f
        camera.contentDescription = if (c.video) "Turn camera off" else "Turn camera on"
        speaker.alpha = if (c.speaker) 1f else 0.6f
        cameraIfWanted()
    }

    private fun duration(ms: Long): String {
        val s = ms / 1000
        return if (s >= 3600) "%d:%02d:%02d".format(s / 3600, s / 60 % 60, s % 60) else "%d:%02d".format(s / 60, s % 60)
    }

    companion object {
        const val ANSWER = "org.threnody.app.ANSWER"
    }

    // The call carries on without the screen; Back just leaves it.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        moveTaskToBack(false)
    }
}
