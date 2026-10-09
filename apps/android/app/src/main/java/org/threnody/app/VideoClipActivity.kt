package org.threnody.app

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.graphics.SurfaceTexture
import android.graphics.drawable.GradientDrawable
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.MediaRecorder
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.view.Gravity
import android.view.Surface
import android.view.TextureView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.io.File

/**
 * Records a video message: our camera's picture with a record button.
 * Stopping returns the file (in the identity's private media folder) and
 * its length to the chat, which sends it; leaving any other way deletes
 * it. The camera and microphone are on only while this screen is.
 */
class VideoClipActivity : Activity() {
    private lateinit var preview: TextureView
    private lateinit var record: ImageButton
    private lateinit var flip: ImageButton
    private lateinit var time: TextView
    private val ui = Handler(Looper.getMainLooper())
    private val thread = HandlerThread("clip-camera").apply { start() }
    private val handler = Handler(thread.looper)
    private var front = true
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var recorder: MediaRecorder? = null
    private var surface: Surface? = null
    private var file: File? = null
    private var startedAt = 0L
    private var recording = false
    private var finished = false

    private val tick = object : Runnable {
        override fun run() {
            if (!recording) return
            val ms = System.currentTimeMillis() - startedAt
            time.text = "● ${Clips.clock(ms)} / ${Clips.clock(Clips.MAX_VIDEO_MS.toLong())}"
            ui.postDelayed(this, 200)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val white = Color.WHITE
        fun round(res: Int, label: String, size: Int, bg: Int, onClick: (View) -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(white)
            contentDescription = label
            tooltipText = label
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(bg)
            }
            layoutParams = LinearLayout.LayoutParams(dp(size), dp(size)).apply {
                marginStart = dp(20)
                marginEnd = dp(20)
            }
            setOnClickListener { v -> Design.mediumHaptic(v); onClick(v) }
        }
        val dim = Color.argb(140, 0, 0, 0)
        preview = TextureView(this).apply {
            surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                override fun onSurfaceTextureAvailable(st: SurfaceTexture, w: Int, h: Int) = open()
                override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, w: Int, h: Int) {}
                override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
                    close()
                    return true
                }
                override fun onSurfaceTextureUpdated(st: SurfaceTexture) {}
            }
        }
        time = TextView(this).apply {
            textSize = 16f
            setTextColor(white)
            gravity = Gravity.CENTER
            setPadding(dp(12), dp(6), dp(12), dp(6))
            background = GradientDrawable().apply {
                cornerRadius = dp(16).toFloat()
                setColor(dim)
            }
            text = "Video message · up to ${Clips.clock(Clips.MAX_VIDEO_MS.toLong())}"
        }
        record = round(R.drawable.ic_videocam, "Record", 72, getColor(R.color.warning)) {
            if (recording) stopAndSend() else start()
        }
        flip = round(R.drawable.ic_cameraswitch, "Switch camera", 52, dim) {
            if (recording) return@round
            front = !front
            close()
            open()
        }
        flip.visibility = if (CallCamera.canSwitch(this)) View.VISIBLE else View.INVISIBLE
        val cancel = round(R.drawable.ic_back, "Cancel", 52, dim) { finish() }
        val controls = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER
            addView(cancel)
            addView(record)
            addView(flip)
        }
        val root = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            // The camera's frames are landscape, turned upright: 3:4.
            addView(preview, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.CENTER))
            addView(time, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.TOP or Gravity.CENTER_HORIZONTAL).apply {
                topMargin = dp(48)
            })
            addView(controls, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.BOTTOM).apply {
                bottomMargin = dp(48)
            })
        }
        root.addOnLayoutChangeListener { _, l, _, r, _, _, _, _, _ ->
            val w = r - l
            val lp = preview.layoutParams
            if (lp.height != w * 4 / 3) {
                lp.height = w * 4 / 3
                preview.layoutParams = lp
            }
        }
        setContentView(root)
    }

    /** Opens the camera with a prepared recorder, showing the preview. */
    @SuppressLint("MissingPermission") // The chat asks for the camera and microphone first.
    private fun open() {
        val st = preview.surfaceTexture ?: return
        val cm = getSystemService(CameraManager::class.java) ?: return fail("no camera")
        val id = CallCamera.idFor(this, front) ?: cm.cameraIdList.firstOrNull() ?: return fail("no camera")
        val chars = cm.getCameraCharacteristics(id)
        val out = Clips.file(this, intent.getStringExtra(PERSONA), video = true)
        val max = intent.getLongExtra(MAX_BYTES, 8L * 1024 * 1024)
        val rec = try {
            Clips.recorder(this).apply {
                setAudioSource(MediaRecorder.AudioSource.MIC)
                setVideoSource(MediaRecorder.VideoSource.SURFACE)
                setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
                setVideoEncoder(MediaRecorder.VideoEncoder.H264)
                setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
                setVideoSize(CallCamera.WIDTH, CallCamera.HEIGHT)
                setVideoFrameRate(30)
                setVideoEncodingBitRate(800_000)
                setAudioChannels(1)
                setAudioSamplingRate(44_100)
                setAudioEncodingBitRate(64_000)
                // Held upright, front or back: the sensor's own angle.
                setOrientationHint(chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0)
                setMaxDuration(Clips.MAX_VIDEO_MS)
                // Some room for the container's index, written at the end.
                setMaxFileSize(max - 256 * 1024)
                setOutputFile(out)
                setOnInfoListener { _, what, _ ->
                    if (what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_DURATION_REACHED ||
                        what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_FILESIZE_REACHED
                    ) ui.post { stopAndSend() }
                }
                prepare()
            }
        } catch (e: Exception) {
            out.delete()
            return fail(e.message ?: "couldn't record")
        }
        recorder = rec
        file = out
        st.setDefaultBufferSize(CallCamera.WIDTH, CallCamera.HEIGHT)
        val shown = Surface(st).also { surface = it }
        // Our picture as in a mirror, as people expect to see themselves.
        preview.scaleX = if (front) -1f else 1f
        val targets = listOf(shown, rec.surface)
        cm.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(d: CameraDevice) {
                device = d
                if (isFinishing) return d.close()
                try {
                    @Suppress("DEPRECATION") // The list form works back to API 29.
                    d.createCaptureSession(targets, object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(s: CameraCaptureSession) {
                            session = s
                            try {
                                val req = d.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                    targets.forEach { addTarget(it) }
                                    set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, android.util.Range(15, 30))
                                }
                                s.setRepeatingRequest(req.build(), null, handler)
                            } catch (e: Exception) {
                                ui.post { fail(e.javaClass.simpleName) }
                            }
                        }

                        override fun onConfigureFailed(s: CameraCaptureSession) {
                            ui.post { fail("couldn't configure the camera") }
                        }
                    }, handler)
                } catch (e: Exception) {
                    ui.post { fail(e.javaClass.simpleName) }
                }
            }

            override fun onDisconnected(d: CameraDevice) = d.close()
            override fun onError(d: CameraDevice, error: Int) {
                d.close()
                ui.post { fail("camera error $error") }
            }
        }, handler)
    }

    private fun start() {
        val rec = recorder ?: return
        try {
            rec.start()
        } catch (e: Exception) {
            return fail(e.message ?: "couldn't record")
        }
        recording = true
        startedAt = System.currentTimeMillis()
        record.setImageResource(R.drawable.ic_stop)
        record.contentDescription = "Stop and send"
        record.tooltipText = "Stop and send"
        flip.visibility = View.INVISIBLE
        ui.post(tick)
    }

    private fun stopAndSend() {
        if (!recording || finished) return
        recording = false
        val ms = (System.currentTimeMillis() - startedAt).coerceAtMost(Clips.MAX_VIDEO_MS.toLong())
        val ok = try {
            recorder?.stop()
            true
        } catch (_: RuntimeException) {
            false
        }
        val out = file
        // Kept (not deleted on closing) only if it's worth sending.
        finished = ok && out != null && ms >= Clips.MIN_MS
        close()
        if (!finished || out == null) {
            Toast.makeText(this, "Too short to send", Toast.LENGTH_SHORT).show()
            return finish()
        }
        setResult(RESULT_OK, Intent().putExtra(RESULT_PATH, out.path).putExtra(RESULT_DURATION_MS, ms))
        finish()
    }

    private fun fail(why: String) {
        Toast.makeText(this, "Couldn't record: $why", Toast.LENGTH_LONG).show()
        finish()
    }

    /** Closes the camera and recorder; an unsent recording is deleted. */
    private fun close() {
        try { session?.close() } catch (_: Exception) {}
        try { device?.close() } catch (_: Exception) {}
        session = null
        device = null
        recorder?.let {
            if (recording) try { it.stop() } catch (_: RuntimeException) {}
            it.release()
        }
        recording = false
        recorder = null
        surface?.release()
        surface = null
        if (!finished) file?.delete()
        file = null
    }

    override fun onPause() {
        super.onPause()
        // Leaving mid-recording (a call, the home button): nothing is sent.
        if (!finished) {
            close()
            finish()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        ui.removeCallbacksAndMessages(null)
        close()
        thread.quitSafely()
    }

    companion object {
        const val PERSONA = "persona"
        const val MAX_BYTES = "max_bytes"
        const val RESULT_PATH = "path"
        const val RESULT_DURATION_MS = "duration_ms"
    }
}
