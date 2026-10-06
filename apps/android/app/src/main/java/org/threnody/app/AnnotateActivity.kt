package org.threnody.app

import android.app.Activity
import android.content.ContentValues
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.net.Uri
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors

/**
 * Allows drawing on a photo with finger, then saves the annotated copy.
 */
class AnnotateActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var image: ImageView
    private lateinit var drawingView: DrawingView
    private var bitmap: Bitmap? = null
    private var location: String = ""
    private var name: String = ""

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)

        location = intent.getStringExtra(LOCATION) ?: return finish()
        name = intent.getStringExtra(NAME) ?: "Photo"

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
        }

        val bar = TopBar(this) { finish() }.apply {
            title.text = "Annotate"
            action(android.R.drawable.ic_menu_save, "Save") { _ -> save() }
            action(android.R.drawable.ic_menu_delete, "Clear") { _ -> drawingView.clear() }
            action(android.R.drawable.ic_menu_close_clear_cancel, "Cancel") { _ -> finish() }
        }

        image = ImageView(this).apply {
            scaleType = ImageView.ScaleType.FIT_CENTER
            contentDescription = "Photo to annotate"
        }
        drawingView = DrawingView(this)

        val frame = FrameLayout(this).apply {
            addView(image, MATCH_PARENT, MATCH_PARENT)
            addView(drawingView, MATCH_PARENT, MATCH_PARENT)
        }

        root.addView(bar, matchWrap)
        root.addView(frame, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)

        fitSystemBars(root, bar, null)

        // Big enough for zooming in, small enough for memory.
        val side = maxOf(resources.displayMetrics.widthPixels, resources.displayMetrics.heightPixels) * 2
        worker.execute {
            val b = Media.decode(Media.source(this, location), side)
            runOnUiThread {
                if (b == null) {
                    Toast.makeText(this, "Couldn't load image", Toast.LENGTH_LONG).show()
                    finish()
                    return@runOnUiThread
                }
                bitmap = b
                image.setImageBitmap(b)
            }
        }
    }

    private fun save() {
        bitmap ?: return
        worker.execute {
            // Create a new bitmap combining the original and the drawing.
            val out = Bitmap.createBitmap(bitmap!!.width, bitmap!!.height, Bitmap.Config.ARGB_8888)
            val canvas = Canvas(out)
            canvas.drawBitmap(bitmap!!, 0f, 0f, null)
            drawingView.drawOnCanvas(canvas)
            val ok = try {
                val outName = name.substringAfterLast('/').let {
                    if (it.contains(".")) it.substringBeforeLast('.') + "-annotated.png" else it + "-annotated.png"
                }
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, outName)
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/Threnody")
                }
                val outUri = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: throw IllegalStateException("no Downloads")
                contentResolver.openOutputStream(outUri)?.use { os ->
                    out.compress(Bitmap.CompressFormat.PNG, 100, os)
                }
                true
            } catch (_: Exception) {
                false
            }
            runOnUiThread {
                Toast.makeText(this, if (ok) "Saved to Downloads/Threnody" else "Couldn't save", Toast.LENGTH_SHORT).show()
                if (ok) finish()
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
    }

    /** A view that lets the user draw with their finger and can render onto a Canvas. */
    class DrawingView(ctx: Activity) : View(ctx) {
        private val paint = Paint().apply {
            color = Color.RED
            isAntiAlias = true
            style = Paint.Style.STROKE
            strokeWidth = 8f
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
        }
        private val paths = mutableListOf<Path>()
        private val currentPath = Path()

        init {
            isFocusable = true
            isFocusableInTouchMode = true
        }

        override fun onTouchEvent(event: MotionEvent): Boolean {
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    currentPath.reset()
                    currentPath.moveTo(event.x, event.y)
                    return true
                }
                MotionEvent.ACTION_MOVE -> {
                    currentPath.lineTo(event.x, event.y)
                    invalidate()
                    return true
                }
                MotionEvent.ACTION_UP -> {
                    paths.add(Path(currentPath))
                    currentPath.reset()
                    invalidate()
                    return true
                }
            }
            return super.onTouchEvent(event)
        }

        override fun onDraw(canvas: Canvas) {
            super.onDraw(canvas)
            paths.forEach { canvas.drawPath(it, paint) }
            canvas.drawPath(currentPath, paint)
        }

        fun clear() {
            paths.clear()
            currentPath.reset()
            invalidate()
        }

        /** Draws the strokes onto a canvas in bitmap coordinates. */
        fun drawOnCanvas(canvas: Canvas) {
            // Map view coordinates to bitmap coordinates.
            val scaleX = width.toFloat() / canvas.width.toFloat()
            val scaleY = height.toFloat() / canvas.height.toFloat()
            val matrix = android.graphics.Matrix().apply {
                postScale(scaleX, scaleY)
            }
            val paint = Paint(paint).apply { setMatrix(matrix) }
            paths.forEach { canvas.drawPath(it, paint) }
            canvas.drawPath(currentPath, paint)
        }
    }

    companion object {
        const val LOCATION = "location"
        const val NAME = "name"
    }
}
