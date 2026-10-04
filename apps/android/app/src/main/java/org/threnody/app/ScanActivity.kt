package org.threnody.app

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.ImageFormat
import android.graphics.Matrix
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.ImageReader
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Size
import android.view.Gravity
import android.view.Surface
import android.view.TextureView
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.Toast
import java.util.concurrent.atomic.AtomicBoolean
import uniffi.threnody_ffi.decodeQr

/**
 * Scans a QR code with the camera (the platform camera2 API; decoding in
 * Rust) and returns its text as [RESULT]. Frames never leave the device
 * or this screen.
 */
class ScanActivity : Activity() {
    private lateinit var preview: TextureView
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null
    private val thread = HandlerThread("threnody-scan").apply { start() }
    private val handler = Handler(thread.looper)
    private val busy = AtomicBoolean(false)
    private val done = AtomicBoolean(false)
    private var size = Size(1280, 720)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val bar = TopBar(this) { finish() }.apply {
            title.text = "Scan a QR code"
            subtitle.text = "An invite or a device link code"
            subtitle.visibility = android.view.View.VISIBLE
        }
        preview = TextureView(this)
        val frame = FrameLayout(this).apply {
            setBackgroundColor(android.graphics.Color.BLACK)
            addView(preview, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT, Gravity.CENTER))
        }
        root.addView(bar, matchWrap)
        root.addView(frame, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, frame)
    }

    override fun onResume() {
        super.onResume()
        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
            return
        }
        whenReady { open() }
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (results.firstOrNull() != PackageManager.PERMISSION_GRANTED) {
            Toast.makeText(this, "Scanning needs the camera. You can paste a link instead.", Toast.LENGTH_LONG).show()
            finish()
        }
    }

    override fun onPause() {
        close()
        super.onPause()
    }

    override fun onDestroy() {
        thread.quitSafely()
        super.onDestroy()
    }

    private fun whenReady(f: () -> Unit) {
        if (preview.isAvailable) return f()
        preview.surfaceTextureListener = object : TextureView.SurfaceTextureListener {
            override fun onSurfaceTextureAvailable(s: SurfaceTexture, w: Int, h: Int) = f()
            override fun onSurfaceTextureSizeChanged(s: SurfaceTexture, w: Int, h: Int) = fit()
            override fun onSurfaceTextureDestroyed(s: SurfaceTexture) = true
            override fun onSurfaceTextureUpdated(s: SurfaceTexture) {}
        }
    }

    @SuppressLint("MissingPermission") // checked in onResume
    private fun open() {
        val cm = getSystemService(CameraManager::class.java) ?: return fail("No camera")
        val id = cm.cameraIdList.firstOrNull {
            cm.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: cm.cameraIdList.firstOrNull() ?: return fail("No camera")
        val map = cm.getCameraCharacteristics(id).get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
        // Around 720p: enough detail for a QR code, cheap to decode.
        size = map?.getOutputSizes(ImageFormat.YUV_420_888)
            ?.filter { it.width <= 1920 && it.height <= 1080 }
            ?.maxByOrNull { it.width * it.height } ?: size
        fit()
        cm.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(c: CameraDevice) {
                camera = c
                start(c)
            }
            override fun onDisconnected(c: CameraDevice) = c.close()
            override fun onError(c: CameraDevice, error: Int) {
                c.close()
                runOnUiThread { fail("Camera error $error") }
            }
        }, handler)
    }

    private fun start(c: CameraDevice) {
        val texture = preview.surfaceTexture ?: return
        texture.setDefaultBufferSize(size.width, size.height)
        val shown = Surface(texture)
        val r = ImageReader.newInstance(size.width, size.height, ImageFormat.YUV_420_888, 2)
        reader = r
        r.setOnImageAvailableListener({ ir ->
            val image = ir.acquireLatestImage() ?: return@setOnImageAvailableListener
            if (done.get() || !busy.compareAndSet(false, true)) {
                image.close()
                return@setOnImageAvailableListener
            }
            // The luma plane is all a QR decoder needs.
            val plane = image.planes[0]
            val buf = plane.buffer
            val luma = ByteArray(buf.remaining()).also { buf.get(it) }
            val (w, h, stride) = Triple(image.width, image.height, plane.rowStride)
            image.close()
            val text = try { decodeQr(w.toUInt(), h.toUInt(), stride.toUInt(), luma) } catch (_: Exception) { null }
            busy.set(false)
            if (text != null && done.compareAndSet(false, true)) runOnUiThread { found(text) }
        }, handler)
        @Suppress("DEPRECATION") // the SessionConfiguration form needs API 28 executors; same effect
        c.createCaptureSession(listOf(shown, r.surface), object : CameraCaptureSession.StateCallback() {
            override fun onConfigured(s: CameraCaptureSession) {
                session = s
                val req = c.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
                    addTarget(shown)
                    addTarget(r.surface)
                    set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE)
                }.build()
                s.setRepeatingRequest(req, null, handler)
            }
            override fun onConfigureFailed(s: CameraCaptureSession) {
                runOnUiThread { fail("Couldn't start the camera") }
            }
        }, handler)
    }

    /** Scales the preview to fill the view without stretching (portrait). */
    private fun fit() {
        val vw = preview.width.toFloat()
        val vh = preview.height.toFloat()
        if (vw == 0f || vh == 0f) return
        // In portrait the sensor's landscape frame is shown rotated, so its
        // width is the long side.
        val (bw, bh) = size.height.toFloat() to size.width.toFloat()
        val scale = maxOf(vw / bw, vh / bh)
        val m = Matrix()
        m.setScale(bw * scale / vw, bh * scale / vh, vw / 2, vh / 2)
        preview.setTransform(m)
    }

    private fun found(text: String) {
        setResult(RESULT_OK, Intent().putExtra(RESULT, text))
        finish()
    }

    private fun fail(msg: String) {
        Toast.makeText(this, msg, Toast.LENGTH_LONG).show()
        finish()
    }

    private fun close() {
        try { session?.close() } catch (_: Exception) {}
        try { camera?.close() } catch (_: Exception) {}
        try { reader?.close() } catch (_: Exception) {}
        session = null
        camera = null
        reader = null
    }

    companion object {
        const val RESULT = "text"
    }
}
