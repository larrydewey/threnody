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
 * The front camera, for video calls: each frame goes to the call as I420
 * (with how far to turn it to be upright), and, if given, to a preview
 * surface for our own picture. Nothing is kept. [stop] closes the camera
 * (the indicator goes off).
 */
class CallCamera(private val ctx: Context, private val node: ThrenodyNode) {
    private val thread = HandlerThread("call-camera").apply { start() }
    private val handler = Handler(thread.looper)
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null
    /** Reused for every frame: width × height luma, then U, then V. */
    private var i420 = ByteArray(0)

    /** Opens the front camera; `preview` (a TextureView's) shows our own picture. */
    @SuppressLint("MissingPermission") // CallActivity asks for the camera first.
    fun start(preview: SurfaceTexture?) {
        val cm = ctx.getSystemService(CameraManager::class.java) ?: return
        val id = cm.cameraIdList.firstOrNull {
            cm.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_FRONT
        } ?: cm.cameraIdList.firstOrNull() ?: return
        val chars = cm.getCameraCharacteristics(id)
        // The phone is held upright (the call screen is portrait): the
        // sensor's own angle is how far to turn each frame.
        val rotation = (chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0).toUInt()
        val r = ImageReader.newInstance(WIDTH, HEIGHT, ImageFormat.YUV_420_888, 2)
        r.setOnImageAvailableListener({ ir ->
            val img = ir.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                node.sendVideoFrame(img.width.toUInt(), img.height.toUInt(), rotation, toI420(img))
            } finally {
                img.close()
            }
        }, handler)
        reader = r
        val surfaces = mutableListOf(r.surface)
        preview?.let {
            it.setDefaultBufferSize(WIDTH, HEIGHT)
            surfaces.add(Surface(it))
        }
        cm.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(d: CameraDevice) {
                device = d
                @Suppress("DEPRECATION") // The list form works back to API 29.
                d.createCaptureSession(surfaces, object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(s: CameraCaptureSession) {
                        session = s
                        val req = d.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                            surfaces.forEach { addTarget(it) }
                            set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, android.util.Range(15, 30))
                        }
                        s.setRepeatingRequest(req.build(), null, handler)
                    }

                    override fun onConfigureFailed(s: CameraCaptureSession) {
                        Threnody.say("! camera: couldn't configure")
                    }
                }, handler)
            }

            override fun onDisconnected(d: CameraDevice) = d.close()
            override fun onError(d: CameraDevice, error: Int) {
                Threnody.say("! camera error $error")
                d.close()
            }
        }, handler)
    }

    fun stop() {
        handler.post {
            session?.close()
            device?.close()
            reader?.close()
            session = null
            device = null
            reader = null
        }
        thread.quitSafely()
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
    }
}
