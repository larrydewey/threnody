package org.threnody.app

import android.app.Activity
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.WindowInsets
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.TextView
import uniffi.threnody_ffi.QrMatrix

fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

fun Context.color(id: Int): Int = getColor(id)

fun rounded(color: Int, radius: Float) = GradientDrawable().apply {
    setColor(color)
    cornerRadius = radius
}

/**
 * Draws the screen edge to edge and pads around the system bars, the
 * display cutout and the keyboard. Bars can sit on any edge: gesture
 * navigation leaves a thin strip at the bottom, three-button navigation a
 * tall bar at the bottom in portrait and on the left or right in landscape.
 * [top] gets the status bar, [bottom] the navigation bar or keyboard
 * (whichever is taller), and [root] the side insets.
 *
 * Android 15+ always lays apps out edge to edge; 11–14 opt in here. On 10
 * the system still lays out around the bars and resizes for the keyboard
 * (`adjustResize`), so nothing is needed.
 */
fun Activity.fitSystemBars(root: View, top: View, bottom: View) {
    if (Build.VERSION.SDK_INT < 30) return
    window.setDecorFitsSystemWindows(false)
    val topPad = top.paddingTop
    val bottomPad = bottom.paddingBottom
    root.setOnApplyWindowInsetsListener { _, insets ->
        val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
        val ime = insets.getInsets(WindowInsets.Type.ime())
        root.setPadding(bars.left, 0, bars.right, 0)
        top.setPadding(top.paddingLeft, topPad + bars.top, top.paddingRight, top.paddingBottom)
        bottom.setPadding(
            bottom.paddingLeft, bottom.paddingTop, bottom.paddingRight,
            bottomPad + maxOf(bars.bottom, ime.bottom),
        )
        WindowInsets.CONSUMED
    }
}

/** A screen's top bar: optional back arrow, title (and subtitle), actions. */
class TopBar(ctx: Context, back: (() -> Unit)?) : LinearLayout(ctx) {
    val title = TextView(ctx).apply {
        textSize = 20f
        setTypeface(typeface, Typeface.BOLD)
        setTextColor(ctx.color(R.color.text))
        isSingleLine = true
    }
    val subtitle = TextView(ctx).apply {
        textSize = 13f
        setTextColor(ctx.color(R.color.muted))
        isSingleLine = true
        visibility = GONE
    }

    init {
        orientation = HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setBackgroundColor(ctx.color(R.color.bar))
        setPadding(ctx.dp(if (back == null) 16 else 4), ctx.dp(8), ctx.dp(4), ctx.dp(8))
        minimumHeight = ctx.dp(56)
        if (back != null) {
            addView(icon(R.drawable.ic_back, "Back") { back() })
        }
        val titles = LinearLayout(ctx).apply {
            orientation = VERTICAL
            addView(title)
            addView(subtitle)
        }
        addView(titles, LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = ctx.dp(if (back == null) 0 else 4) })
    }

    fun icon(res: Int, label: String, onClick: (View) -> Unit) = ImageButton(context).apply {
        setImageResource(res)
        imageTintList = android.content.res.ColorStateList.valueOf(context.color(R.color.text))
        contentDescription = label
        tooltipText = label
        background = context.getDrawable(android.R.drawable.list_selector_background)
        layoutParams = LayoutParams(context.dp(48), context.dp(48))
        setOnClickListener(onClick)
    }

    fun action(res: Int, label: String, onClick: (View) -> Unit) {
        addView(icon(res, label, onClick))
    }
}

/** A circle with a contact's initial, tinted from its fingerprint. */
class Avatar(ctx: Context, size: Int) : TextView(ctx) {
    init {
        gravity = Gravity.CENTER
        textSize = size / 2.6f
        setTextColor(Color.WHITE)
        setTypeface(typeface, Typeface.BOLD)
        layoutParams = LinearLayout.LayoutParams(ctx.dp(size), ctx.dp(size))
    }

    fun show(title: String, fingerprint: String) {
        text = title.trim().take(1).uppercase()
        val hue = (fingerprint.hashCode().toLong() and 0xffff).toFloat() % 360f
        background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(Color.HSVToColor(floatArrayOf(hue, 0.45f, 0.62f)))
        }
    }
}

/** Draws a QR code, always dark on white with a quiet zone, as scanners expect. */
class QrView(ctx: Context, private val qr: QrMatrix) : View(ctx) {
    private val paint = Paint().apply { color = Color.BLACK }

    init {
        setBackgroundColor(Color.WHITE)
        contentDescription = "QR code"
    }

    override fun onMeasure(w: Int, h: Int) {
        val side = minOf(MeasureSpec.getSize(w), context.dp(280))
        setMeasuredDimension(side, side)
    }

    override fun onDraw(canvas: Canvas) {
        val n = qr.size.toInt()
        val cell = width.toFloat() / (n + 8)
        for (y in 0 until n) for (x in 0 until n) {
            if (qr.dark[y * n + x]) {
                val l = (x + 4) * cell
                val t = (y + 4) * cell
                canvas.drawRect(l, t, l + cell + 0.5f, t + cell + 0.5f, paint)
            }
        }
    }
}

fun Context.label(text: String, size: Float = 15f, colorId: Int = R.color.text) = TextView(this).apply {
    this.text = text
    textSize = size
    setTextColor(color(colorId))
}

val matchWrap get() = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)

fun members(n: Int) = if (n == 1) "1 member" else "$n members"
