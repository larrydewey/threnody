package org.threnody.app

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.ImageFormat
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import uniffi.threnody_ffi.ThrenodyNode

/**
 * A camera, for video calls: each frame goes to the call as I420 (with how
 * far to turn it to be upright), and, if given, to a preview surface for
 * our own picture. Nothing is kept. [stop] closes the camera (the
 * indicator goes off).
 */
class CallCamera(private val ctx: Context, private val node: ThrenodyNode, private val front: Boolean = true) {
    private val thread = HandlerThread("call-camera").apply { start() }
    private val handler = Handler(thread.looper)
    // Frames on a thread of their own: closing the camera waits for its
    // buffers to come back, which can't happen on the thread doing the
    // closing.
    private val frameThread = HandlerThread("call-camera-frames").apply { start() }
    private val frameHandler = Handler(frameThread.looper)
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null
    /** Our preview's surface: released with the camera, so the next one can use the view. */
    private var previewSurface: Surface? = null
    /** Set by [stop]: callbacks still on their way find the camera gone. */
    @Volatile private var stopped = false
    /** Counted down once the camera has really closed (not just been asked to). */
    private val closed = java.util.concurrent.CountDownLatch(1)
    /** Reused for every frame: width × height luma, then U, then V. */
    private var i420 = ByteArray(0)

    /** Opens the front (or back) camera; `preview` (a TextureView's) shows our own picture. */
    @SuppressLint("MissingPermission") // CallActivity asks for the camera first.
    fun start(preview: SurfaceTexture?) {
        val cm = ctx.getSystemService(CameraManager::class.java) ?: return
        val id = idFor(ctx, front) ?: cm.cameraIdList.firstOrNull() ?: return
        val chars = cm.getCameraCharacteristics(id)
        // The phone is held upright (the call screen is portrait): the
        // sensor's own angle is how far to turn each frame, front or back.
        val rotation = (chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0).toUInt()
        val r = ImageReader.newInstance(WIDTH, HEIGHT, ImageFormat.YUV_420_888, 2)
        r.setOnImageAvailableListener({ ir ->
            val img = ir.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                node.sendVideoFrame(img.width.toUInt(), img.height.toUInt(), rotation, toI420(img))
            } finally {
                img.close()
            }
        }, frameHandler)
        reader = r
        val surfaces = mutableListOf(r.surface)
        preview?.let {
            it.setDefaultBufferSize(WIDTH, HEIGHT)
            surfaces.add(Surface(it).also { s -> previewSurface = s })
        }
        cm.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(d: CameraDevice) {
                device = d
                if (stopped) return d.close()
                try {
                    @Suppress("DEPRECATION") // The list form works back to API 29.
                    d.createCaptureSession(surfaces, object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(s: CameraCaptureSession) {
                            session = s
                            // Stopped meanwhile (the screen went): its preview is gone.
                            if (stopped) return s.close()
                            try {
                                val req = d.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                    surfaces.forEach { addTarget(it) }
                                    set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, android.util.Range(15, 30))
                                }
                                s.setRepeatingRequest(req.build(), null, handler)
                            } catch (e: Exception) {
                                Threnody.say("! camera: ${e.javaClass.simpleName}")
                            }
                        }

                        override fun onConfigureFailed(s: CameraCaptureSession) {
                            if (!stopped) Threnody.say("! camera: couldn't configure")
                        }
                    }, handler)
                } catch (e: Exception) {
                    Threnody.say("! camera: ${e.javaClass.simpleName}")
                }
            }

            override fun onClosed(d: CameraDevice) = closed.countDown()
            override fun onDisconnected(d: CameraDevice) = d.close()
            override fun onError(d: CameraDevice, error: Int) {
                Threnody.say("! camera error $error")
                d.close()
            }
        }, handler)
    }

    /**
     * Closes the camera and waits (briefly) until it is: another camera,
     * opened next (a switch), often can't start while this one holds the
     * camera system.
     */
    fun stop() {
        stopped = true
        handler.post {
            session?.close()
            // onClosed follows, on this thread, once it has let go.
            device?.close() ?: closed.countDown()
            session = null
            device = null
        }
        closed.await(1500, java.util.concurrent.TimeUnit.MILLISECONDS)
        frameHandler.post {
            reader?.close()
            previewSurface?.release()
            reader = null
            previewSurface = null
        }
        thread.quitSafely()
        frameThread.quitSafely()
    }

    /** Copies a YUV_420_888 image (any strides) into I420. */
    private fun toI420(img: Image): ByteArray {
        val (w, h) = img.width to img.height
        val (cw, ch) = (w + 1) / 2 to (h + 1) / 2
        val size = w * h + 2 * cw * ch
        if (i420.size != size) i420 = ByteArray(size)
        var at = 0
        for ((i, plane) in img.planes.withIndex()) {
            val (pw, ph) = if (i == 0) w to h else cw to ch
            val buf = plane.buffer
            val (row, px) = plane.rowStride to plane.pixelStride
            if (px == 1) {
                for (y in 0 until ph) {
                    buf.position(y * row)
                    buf.get(i420, at, pw)
                    at += pw
                }
            } else {
                for (y in 0 until ph) {
                    val base = y * row
                    for (x in 0 until pw) i420[at++] = buf.get(base + x * px)
                }
            }
        }
        return i420
    }

    companion object {
        const val WIDTH = 640
        const val HEIGHT = 480

        /** The first front (or back) camera, if the phone has one. */
        fun idFor(ctx: Context, front: Boolean): String? {
            val cm = ctx.getSystemService(CameraManager::class.java) ?: return null
            val want = if (front) CameraCharacteristics.LENS_FACING_FRONT else CameraCharacteristics.LENS_FACING_BACK
            return try {
                cm.cameraIdList.firstOrNull { cm.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == want }
            } catch (_: Exception) { null }
        }

        /** Whether there's both a front and a back camera to switch between. */
        fun canSwitch(ctx: Context) = idFor(ctx, true) != null && idFor(ctx, false) != null
    }
}
