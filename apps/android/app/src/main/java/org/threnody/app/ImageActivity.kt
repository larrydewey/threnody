package org.threnody.app

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ContentValues
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.RectF
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.view.GestureDetector
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors

/**
 * Shows one photo full screen, inside the app (so a sensitive one isn't
 * handed to another app), with pinch and double-tap zoom. Saving a copy
 * to Downloads is an explicit choice.
 */
class ImageActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var image: ImageView
    private val matrix = Matrix()
    private var bitmap: Bitmap? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val location = intent.getStringExtra(LOCATION) ?: return finish()
        val name = intent.getStringExtra(NAME) ?: "Photo"
        val caption = intent.getStringExtra(CAPTION).orEmpty()

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
        }
        val bar = TopBar(this) { finish() }.apply {
            title.text = name
            action(R.drawable.ic_more, "Photo options") { anchor ->
                android.widget.PopupMenu(this@ImageActivity, anchor).apply {
                    menu.add("Save to Downloads").setOnMenuItemClickListener { save(location, name); true }
                }.show()
            }
        }
        image = ImageView(this).apply {
            scaleType = ImageView.ScaleType.MATRIX
            contentDescription = caption.ifBlank { name }
        }
        val frame = FrameLayout(this).apply { addView(image, MATCH_PARENT, MATCH_PARENT) }
        root.addView(bar, matchWrap)
        root.addView(frame, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        val captionView = TextView(this).apply {
            text = caption
            textSize = 16f
            setTextColor(Color.WHITE)
            setPadding(dp(16), dp(12), dp(16), dp(12))
            visibility = if (caption.isBlank()) View.GONE else View.VISIBLE
        }
        root.addView(captionView, matchWrap)
        setContentView(root)
        fitSystemBars(root, bar, captionView)
        zoom()

        // Big enough for zooming in, small enough for memory.
        val side = maxOf(resources.displayMetrics.widthPixels, resources.displayMetrics.heightPixels) * 2
        worker.execute {
            val b = Media.decode(Media.source(this, location), side)
            runOnUiThread {
                if (b == null) {
                    Toast.makeText(this, "Couldn't show this image", Toast.LENGTH_LONG).show()
                    finish()
                    return@runOnUiThread
                }
                bitmap = b
                image.setImageBitmap(b)
                image.post { fit() }
            }
        }
    }

    /** Fits the whole image in view. */
    private fun fit() {
        val b = bitmap ?: return
        matrix.setRectToRect(
            RectF(0f, 0f, b.width.toFloat(), b.height.toFloat()),
            RectF(0f, 0f, image.width.toFloat(), image.height.toFloat()),
            Matrix.ScaleToFit.CENTER,
        )
        image.imageMatrix = matrix
    }

    @SuppressLint("ClickableViewAccessibility")
    private fun zoom() {
        val scale = ScaleGestureDetector(this, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScale(d: ScaleGestureDetector): Boolean {
                matrix.postScale(d.scaleFactor, d.scaleFactor, d.focusX, d.focusY)
                image.imageMatrix = matrix
                return true
            }
        })
        var zoomed = false
        val gestures = GestureDetector(this, object : GestureDetector.SimpleOnGestureListener() {
            override fun onScroll(e1: MotionEvent?, e2: MotionEvent, dx: Float, dy: Float): Boolean {
                matrix.postTranslate(-dx, -dy)
                image.imageMatrix = matrix
                return true
            }

            override fun onDoubleTap(e: MotionEvent): Boolean {
                zoomed = !zoomed
                if (zoomed) {
                    matrix.postScale(2.5f, 2.5f, e.x, e.y)
                    image.imageMatrix = matrix
                } else {
                    fit()
                }
                return true
            }
        })
        image.setOnTouchListener { _, ev ->
            scale.onTouchEvent(ev)
            gestures.onTouchEvent(ev)
            true
        }
    }

    private fun save(location: String, name: String) {
        worker.execute {
            val ok = try {
                val uri = android.net.Uri.parse(location)
                val bytes = (if (uri.scheme == "file") java.io.File(uri.path ?: "").readBytes()
                    else contentResolver.openInputStream(uri)?.use { it.readBytes() })
                    ?: throw IllegalStateException("unreadable")
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, name.substringAfterLast('/').ifBlank { "photo" })
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/Threnody")
                }
                val out = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: throw IllegalStateException("no Downloads")
                contentResolver.openOutputStream(out)?.use { it.write(bytes) }
                true
            } catch (_: Exception) {
                false
            }
            runOnUiThread {
                Toast.makeText(this, if (ok) "Saved to Downloads/Threnody" else "Couldn't save", Toast.LENGTH_SHORT).show()
            }
        }
    }

    override fun onDestroy() {
        worker.shutdownNow()
        super.onDestroy()
    }

    companion object {
        const val LOCATION = "location"
        const val NAME = "name"
        const val CAPTION = "caption"
    }
}
