package org.threnody.app

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ContentValues
import android.content.Intent
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
            // An edited GIF would be a still photo: GIFs aren't edited.
            if (!Media.isGif(name)) action(R.drawable.ic_edit, "Edit: draw or add text") {
                startActivityForResult(
                    Intent(this@ImageActivity, AnnotateActivity::class.java)
                        .putExtra(AnnotateActivity.LOCATION, location)
                        .putExtra(AnnotateActivity.NAME, name),
                    EDIT,
                )
            }
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
        if (Media.isGif(name)) {
            Media.animated(this, location, side) { d ->
                if (d == null) return@animated still(location, side)
                runOnUiThread {
                    image.setImageDrawable(d)
                    (d as android.graphics.drawable.AnimatedImageDrawable).start()
                    image.post { fit() }
                }
            }
        } else {
            still(location, side)
        }
    }

    private fun still(location: String, side: Int) {
        worker.execute {
            val b = Media.decode(Media.source(this, location), side)
            runOnUiThread {
                if (b == null) {
                    Toast.makeText(this, "Couldn't show this image", Toast.LENGTH_LONG).show()
                    finish()
                    return@runOnUiThread
                }
                image.setImageBitmap(b)
                image.post { fit() }
            }
        }
    }

    /** Fits the whole image in view. */
    private fun fit() {
        val d = image.drawable ?: return
        matrix.setRectToRect(
            RectF(0f, 0f, d.intrinsicWidth.toFloat(), d.intrinsicHeight.toFloat()),
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

    @Deprecated("Activity result API needs AndroidX; this app uses the platform only.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val path = data?.getStringExtra(AnnotateActivity.RESULT_PATH)
        if (requestCode != EDIT || resultCode != RESULT_OK || path == null) return
        val name = intent.getStringExtra(NAME) ?: "photo"
        val edited = name.substringAfterLast('/').substringBeforeLast('.') + "-edited.jpg"
        val f = java.io.File(path)
        val saveIt = { save(android.net.Uri.fromFile(f).toString(), edited, f) }
        // Opened from a chat: the edited copy can go straight back there.
        if (callingActivity == null) return saveIt()
        SecureBuilder(this)
            .setTitle("Edited photo")
            .setItems(arrayOf("Send in this chat", "Save to Downloads")) { _, which ->
                if (which == 0) {
                    setResult(RESULT_OK, Intent().putExtra(AnnotateActivity.RESULT_PATH, path).putExtra(NAME, name))
                    finish()
                } else {
                    saveIt()
                }
            }
            .setNegativeButton("Discard") { _, _ -> f.delete() }
            .show()
    }

    /** Copies the image at `location` to Downloads; deletes `then` afterwards. */
    private fun save(location: String, name: String, then: java.io.File? = null) {
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
            then?.delete()
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
        private const val EDIT = 1
    }
}
