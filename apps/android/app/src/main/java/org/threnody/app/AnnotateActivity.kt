package org.threnody.app

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.DashPathEffect
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.ViewConfiguration
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.WindowManager
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.io.File
import kotlin.math.abs

/**
 * A photo editor: draw with a finger, or place text, in a choice of
 * colours. Done hands back the edited copy as a private file
 * ([RESULT_PATH]); the caller sends or saves it.
 */
class AnnotateActivity : Activity() {
    private lateinit var canvasView: EditorView
    private lateinit var undoButton: View
    private lateinit var hint: TextView
    private lateinit var sizes: LinearLayout
    private val toolChips = mutableMapOf<Tool, TextView>()
    private val swatches = mutableListOf<View>()
    private var name: String = ""

    enum class Tool { DRAW, TEXT }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val location = intent.getStringExtra(LOCATION) ?: return finish()
        name = intent.getStringExtra(NAME) ?: "Photo"

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
        }
        canvasView = EditorView(this, ::askText) { refreshUndo() }
        val bar = TopBar(this) { cancel() }.apply {
            title.text = "Edit photo"
            action(R.drawable.ic_undo, "Undo") { canvasView.undo() }
            undoButton = getChildAt(childCount - 1)
            addView(TextView(context).apply {
                text = "Done"
                textSize = 16f
                setTypeface(typeface, Typeface.BOLD)
                setTextColor(color(R.color.on_accent))
                background = rounded(color(R.color.accent), dp(18).toFloat())
                setPadding(dp(18), dp(8), dp(18), dp(8))
                contentDescription = "Done editing"
                setOnClickListener { done() }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginStart = dp(4); marginEnd = dp(8) })
        }
        val frame = FrameLayout(this).apply { addView(canvasView, MATCH_PARENT, MATCH_PARENT) }

        val panel = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.rgb(0x16, 0x16, 0x1A))
            setPadding(dp(12), dp(10), dp(12), dp(10))
        }
        // Tools on the left, pen sizes (drawing only) on the right.
        val tools = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
        }
        for ((tool, label) in listOf(Tool.DRAW to "✏️  Draw", Tool.TEXT to "Aa  Text")) {
            val chip = TextView(this).apply {
                text = label
                textSize = 15f
                setTypeface(typeface, Typeface.BOLD)
                gravity = Gravity.CENTER
                setPadding(dp(16), dp(8), dp(16), dp(8))
                setOnClickListener { setTool(tool) }
            }
            toolChips[tool] = chip
            tools.addView(chip, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginEnd = dp(8) })
        }
        tools.addView(View(this), LinearLayout.LayoutParams(0, 1, 1f))
        sizes = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
        }
        for ((i, w) in PEN_DP.withIndex()) {
            sizes.addView(FrameLayout(this).apply {
                contentDescription = listOf("Thin pen", "Medium pen", "Thick pen")[i]
                addView(View(context).apply {
                    background = GradientDrawable().apply { shape = GradientDrawable.OVAL; setColor(Color.WHITE) }
                }, FrameLayout.LayoutParams(dp(w + 4), dp(w + 4), Gravity.CENTER))
                setOnClickListener { setPen(i) }
            }, LinearLayout.LayoutParams(dp(40), dp(40)))
        }
        tools.addView(sizes)
        panel.addView(tools, matchWrap)

        val colours = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER
            setPadding(0, dp(10), 0, dp(4))
        }
        for (c in COLORS) {
            val swatch = View(this).apply {
                contentDescription = "Colour"
                tag = c
                setOnClickListener { setColour(c) }
            }
            swatches.add(swatch)
            colours.addView(swatch, LinearLayout.LayoutParams(dp(34), dp(34)).apply { marginStart = dp(7); marginEnd = dp(7) })
        }
        panel.addView(colours, matchWrap)
        hint = TextView(this).apply {
            textSize = 12f
            setTextColor(Color.rgb(0xB0, 0xB0, 0xB8))
            gravity = Gravity.CENTER
        }
        panel.addView(hint, matchWrap)

        root.addView(bar, matchWrap)
        root.addView(frame, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        root.addView(panel, matchWrap)
        setContentView(root)
        fitSystemBars(root, bar, panel)

        setColour(COLORS.first())
        setPen(1)
        setTool(Tool.DRAW)
        refreshUndo()

        // Full quality for sending, bounded for memory.
        Threading.background {
            val b = Media.decode(Media.source(this, location), 4096)
            runOnUiThread {
                if (b == null) {
                    Toast.makeText(this, "Couldn't open this image", Toast.LENGTH_LONG).show()
                    finish()
                } else {
                    canvasView.bitmap = b
                }
            }
        }
    }

    private fun setTool(tool: Tool) {
        canvasView.tool = tool
        for ((t, chip) in toolChips) {
            val on = t == tool
            chip.background = if (on) rounded(Color.WHITE, dp(18).toFloat()) else null
            chip.setTextColor(if (on) Color.BLACK else Color.WHITE)
        }
        sizes.visibility = if (tool == Tool.DRAW) View.VISIBLE else View.INVISIBLE
        hint.text = when (tool) {
            Tool.DRAW -> "Draw on the photo with your finger"
            Tool.TEXT -> "Tap the photo to add text. Drag text to move it, pinch to resize, tap to change it."
        }
    }

    private fun setColour(c: Int) {
        canvasView.setColour(c)
        for (s in swatches) {
            val on = s.tag == c
            s.background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(s.tag as Int)
                setStroke(dp(if (on) 3 else 1), if (on) Color.WHITE else Color.GRAY)
            }
            s.scaleX = if (on) 1.15f else 1f
            s.scaleY = s.scaleX
        }
    }

    private fun setPen(i: Int) {
        canvasView.penDp = PEN_DP[i]
        for (j in 0 until sizes.childCount) sizes.getChildAt(j).alpha = if (j == i) 1f else 0.45f
    }

    private fun refreshUndo() {
        undoButton.isEnabled = canvasView.canUndo
        undoButton.alpha = if (canvasView.canUndo) 1f else 0.35f
    }

    /** Asks for new text (at a point) or a change to `existing`. */
    private fun askText(existing: EditorView.Label?, onDone: (String) -> Unit) {
        val input = EditText(this).apply {
            setText(existing?.text.orEmpty())
            setSelection(text.length)
            hint = "Your text"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 6)
        }
        val box = FrameLayout(this).apply {
            setPadding(dp(20), dp(8), dp(20), 0)
            addView(input)
        }
        val dialog = SecureBuilder(this)
            .setTitle(if (existing == null) "Add text" else "Change text")
            .setView(box)
            .setPositiveButton("Done") { _, _ -> onDone(input.text.toString().trim()) }
            .setNegativeButton("Cancel", null)
            .apply { if (existing != null) setNeutralButton("Remove") { _, _ -> onDone("") } }
            .create()
        dialog.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
        dialog.show()
        input.requestFocus()
    }

    private fun cancel() {
        if (!canvasView.canUndo) return finish()
        SecureBuilder(this)
            .setTitle("Discard your changes?")
            .setPositiveButton("Discard") { _, _ -> finish() }
            .setNegativeButton("Keep editing", null)
            .show()
    }

    @Deprecated("Platform back handling; this app uses the platform only.")
    override fun onBackPressed() = cancel()

    private fun done() {
        val out = canvasView.render() ?: return
        val base = name.substringAfterLast('/').substringBeforeLast('.').ifBlank { "photo" }
        Threading.background {
            val path = try {
                // Private; the caller deletes it once used.
                val dir = File(cacheDir, DIR).apply { mkdirs() }
                val f = File(dir, "$base-edited.jpg")
                f.outputStream().use { out.compress(Bitmap.CompressFormat.JPEG, 92, it) }
                f.path
            } catch (_: Exception) {
                null
            }
            runOnUiThread {
                if (path == null) {
                    Toast.makeText(this, "Couldn't save the edited photo", Toast.LENGTH_SHORT).show()
                } else {
                    setResult(RESULT_OK, Intent().putExtra(RESULT_PATH, path).putExtra(INDEX, intent.getIntExtra(INDEX, -1)))
                    finish()
                }
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
    }

    /**
     * Shows the photo fitted to the view and keeps marks in the photo's own
     * pixels, so the saved copy matches what was drawn.
     */
    @SuppressLint("ViewConstructor")
    class EditorView(
        ctx: Context,
        private val ask: (Label?, (String) -> Unit) -> Unit,
        private val changed: () -> Unit,
    ) : View(ctx) {
        sealed class Mark
        class Stroke(val path: Path, val color: Int, val width: Float) : Mark()
        class Label(var text: String, var color: Int, var x: Float, var y: Float, var size: Float) : Mark()

        var bitmap: Bitmap? = null
            set(b) { field = b; fit(); invalidate() }
        var tool = Tool.DRAW
            set(t) { field = t; selected = null; invalidate() }
        var penDp = 6
        private var colour = Color.RED
        private val marks = mutableListOf<Mark>()
        private val undos = mutableListOf<() -> Unit>()
        val canUndo get() = undos.isNotEmpty()

        private var stroke: Stroke? = null
        /** The text last touched: a colour choice applies to it. */
        private var selected: Label? = null
        private var dragging: Label? = null
        private var downX = 0f
        private var downY = 0f
        private var moved = false
        private var before: Triple<Float, Float, Float>? = null
        private val slop = ViewConfiguration.get(ctx).scaledTouchSlop

        private val toView = Matrix()
        private val toBitmap = Matrix()
        private val ink = Paint().apply {
            isAntiAlias = true
            style = Paint.Style.STROKE
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
        }
        private val type = Paint().apply {
            isAntiAlias = true
            textAlign = Paint.Align.CENTER
            typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        }
        private val outline = Paint(type).apply {
            style = Paint.Style.STROKE
            strokeJoin = Paint.Join.ROUND
        }
        private val frame = Paint().apply {
            isAntiAlias = true
            style = Paint.Style.STROKE
            color = Color.WHITE
            strokeWidth = ctx.dp(2).toFloat()
            pathEffect = DashPathEffect(floatArrayOf(ctx.dp(6).toFloat(), ctx.dp(4).toFloat()), 0f)
        }

        private val pinch = ScaleGestureDetector(ctx, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScaleBegin(d: ScaleGestureDetector): Boolean {
                // Pinching anywhere resizes the selected text.
                if (dragging == null) {
                    val l = selected?.takeIf { it in marks } ?: return false
                    dragging = l
                    before = Triple(l.x, l.y, l.size)
                }
                return true
            }

            override fun onScale(d: ScaleGestureDetector): Boolean {
                val l = dragging ?: return false
                l.size = (l.size * d.scaleFactor).coerceIn(toBitmap.mapRadius(context.dp(12).toFloat()), toBitmap.mapRadius(context.dp(160).toFloat()))
                moved = true
                invalidate()
                return true
            }
        })

        fun setColour(c: Int) {
            colour = c
            val l = selected ?: return
            if (l.color == c) return
            val old = l.color
            l.color = c
            record { l.color = old }
            invalidate()
        }

        private fun record(undo: () -> Unit) {
            undos.add(undo)
            changed()
        }

        fun undo() {
            undos.removeLastOrNull()?.invoke()
            stroke = null
            if (selected?.let { it !in marks } == true) selected = null
            changed()
            invalidate()
        }

        override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) = fit()

        private fun fit() {
            val b = bitmap ?: return
            if (width == 0 || height == 0) return
            toView.setRectToRect(
                RectF(0f, 0f, b.width.toFloat(), b.height.toFloat()),
                RectF(0f, 0f, width.toFloat(), height.toFloat()),
                Matrix.ScaleToFit.CENTER,
            )
            toView.invert(toBitmap)
        }

        private fun toPhoto(x: Float, y: Float) = floatArrayOf(x, y).also { toBitmap.mapPoints(it) }

        @SuppressLint("ClickableViewAccessibility")
        override fun onTouchEvent(event: MotionEvent): Boolean {
            if (bitmap == null) return false
            return if (tool == Tool.DRAW) draw(event) else text(event)
        }

        private fun draw(event: MotionEvent): Boolean {
            val p = toPhoto(event.x, event.y)
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    // A steady width on screen, whatever the photo's size.
                    val s = Stroke(Path().apply { moveTo(p[0], p[1]) }, colour, toBitmap.mapRadius(context.dp(penDp).toFloat()))
                    stroke = s
                    marks.add(s)
                }
                MotionEvent.ACTION_MOVE -> stroke?.path?.lineTo(p[0], p[1])
                MotionEvent.ACTION_UP -> stroke?.let { s ->
                    // A tap leaves a dot.
                    s.path.lineTo(p[0] + 0.1f, p[1])
                    stroke = null
                    record { marks.remove(s) }
                }
                MotionEvent.ACTION_CANCEL -> stroke?.let { marks.remove(it); stroke = null }
                else -> return true
            }
            invalidate()
            return true
        }

        private fun text(event: MotionEvent): Boolean {
            pinch.onTouchEvent(event)
            val p = toPhoto(event.x, event.y)
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    downX = event.x
                    downY = event.y
                    moved = false
                    dragging = labelAt(p[0], p[1])
                    dragging?.let { before = Triple(it.x, it.y, it.size) }
                    selected = dragging
                    invalidate()
                }
                MotionEvent.ACTION_MOVE -> {
                    val l = dragging ?: return true
                    if (!moved && abs(event.x - downX) < slop && abs(event.y - downY) < slop) return true
                    if (event.pointerCount > 1) return true
                    if (!moved) {
                        moved = true
                        downX = event.x
                        downY = event.y
                    }
                    val d = toBitmap.mapRadius(1f)
                    l.x += (event.x - downX) * d
                    l.y += (event.y - downY) * d
                    downX = event.x
                    downY = event.y
                    invalidate()
                }
                MotionEvent.ACTION_UP -> {
                    val l = dragging
                    dragging = null
                    when {
                        l != null && moved -> {
                            val (x, y, s) = before!!
                            record { l.x = x; l.y = y; l.size = s }
                        }
                        l != null -> edit(l)
                        else -> add(p[0], p[1])
                    }
                }
                MotionEvent.ACTION_POINTER_UP -> {
                    // Carry on from the finger that stays.
                    val i = if (event.actionIndex == 0) 1 else 0
                    downX = event.getX(i)
                    downY = event.getY(i)
                }
                MotionEvent.ACTION_CANCEL -> dragging = null
            }
            return true
        }

        private fun add(x: Float, y: Float) {
            ask(null) { text ->
                if (text.isEmpty()) return@ask
                val l = Label(text, colour, x, y, toBitmap.mapRadius(context.dp(30).toFloat()))
                marks.add(l)
                selected = l
                record { marks.remove(l) }
                invalidate()
            }
        }

        private fun edit(l: Label) {
            ask(l) { text ->
                val old = l.text
                if (text.isEmpty()) {
                    val i = marks.indexOf(l)
                    marks.remove(l)
                    selected = null
                    record { marks.add(i.coerceIn(0, marks.size), l) }
                } else if (text != old) {
                    l.text = text
                    record { l.text = old }
                }
                invalidate()
            }
        }

        private fun labelAt(x: Float, y: Float): Label? {
            val pad = toBitmap.mapRadius(context.dp(12).toFloat())
            return marks.filterIsInstance<Label>().lastOrNull { bounds(it).apply { inset(-pad, -pad) }.contains(x, y) }
        }

        /** Where a label's text lies, in photo pixels. */
        private fun bounds(l: Label): RectF {
            type.textSize = l.size
            val lines = l.text.lines()
            val w = lines.maxOf { type.measureText(it) }
            val h = lines.size * l.size * LINE
            return RectF(l.x - w / 2, l.y - h / 2, l.x + w / 2, l.y + h / 2)
        }

        override fun onDraw(canvas: Canvas) {
            val b = bitmap ?: return
            canvas.save()
            canvas.concat(toView)
            canvas.drawBitmap(b, 0f, 0f, null)
            drawMarks(canvas)
            canvas.restore()
            val l = selected
            if (tool == Tool.TEXT && l != null && l in marks) {
                val r = bounds(l)
                toView.mapRect(r)
                r.inset(-context.dp(8).toFloat(), -context.dp(6).toFloat())
                canvas.drawRoundRect(r, context.dp(6).toFloat(), context.dp(6).toFloat(), frame)
            }
        }

        private fun drawMarks(canvas: Canvas) {
            for (m in marks) when (m) {
                is Stroke -> {
                    ink.color = m.color
                    ink.strokeWidth = m.width
                    canvas.drawPath(m.path, ink)
                }
                is Label -> {
                    type.textSize = m.size
                    type.color = m.color
                    outline.textSize = m.size
                    outline.strokeWidth = m.size * 0.14f
                    // A contrasting edge keeps text legible on any photo.
                    outline.color = if (Color.luminance(m.color) > 0.5f) Color.argb(200, 0, 0, 0) else Color.argb(220, 255, 255, 255)
                    val lines = m.text.lines()
                    val top = m.y - lines.size * m.size * LINE / 2
                    for ((i, line) in lines.withIndex()) {
                        // Baseline: a line's height down, less the descent.
                        val y = top + (i + 1) * m.size * LINE - m.size * 0.3f
                        canvas.drawText(line, m.x, y, outline)
                        canvas.drawText(line, m.x, y, type)
                    }
                }
            }
        }

        /** The photo with its marks, at the photo's size. */
        fun render(): Bitmap? {
            val b = bitmap ?: return null
            val out = b.copy(Bitmap.Config.ARGB_8888, true)
            drawMarks(Canvas(out))
            return out
        }

        private companion object {
            const val LINE = 1.2f
        }
    }

    companion object {
        const val LOCATION = "location"
        const val NAME = "name"
        /** Which picked file this is, handed back with the result. */
        const val INDEX = "index"
        const val RESULT_PATH = "path"
        /** Where edited photos wait in the cache until used. */
        const val DIR = "edited"
        private val PEN_DP = listOf(3, 6, 12)
        private val COLORS = listOf(
            Color.WHITE, Color.BLACK, Color.rgb(0xE5, 0x39, 0x35), Color.rgb(0xFB, 0x8C, 0x00),
            Color.rgb(0xFD, 0xD8, 0x35), Color.rgb(0x43, 0xA0, 0x47), Color.rgb(0x1E, 0x88, 0xE5),
            Color.rgb(0x8E, 0x24, 0xAA),
        )
    }
}
