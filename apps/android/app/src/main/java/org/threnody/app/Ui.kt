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
import android.view.ViewConfiguration
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.WindowInsets
import android.text.InputType
import android.view.inputmethod.EditorInfo
import android.widget.EditText
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.TextView
import uniffi.threnody_ffi.QrMatrix

fun Context.dp(v: Float): Int = (v * resources.displayMetrics.density).toInt()
fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

fun Context.color(id: Int): Int = getColor(id)

/** The system's touch ripple: bounded for rows, borderless for icons. */
fun Context.ripple(borderless: Boolean = false): android.graphics.drawable.Drawable? {
    val v = android.util.TypedValue()
    theme.resolveAttribute(
        if (borderless) android.R.attr.selectableItemBackgroundBorderless else android.R.attr.selectableItemBackground, v, true,
    )
    return getDrawable(v.resourceId)
}

fun rounded(color: Int, radius: Float = Design.radiusMd) = GradientDrawable().apply {
    setColor(color)
    cornerRadius = radius
}

fun Context.roundedRes(colorId: Int, radius: Float = Design.radiusMd): GradientDrawable {
    val ctx = this
    return GradientDrawable().apply {
        setColor(ctx.color(colorId))
        cornerRadius = radius
    }
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
        textSize = Design.typeTitle
        setTypeface(typeface, Design.weightBold)
        setTextColor(ctx.color(R.color.text))
        isSingleLine = true
    }
    val subtitle = TextView(ctx).apply {
        textSize = Design.typeBodySmall
        setTextColor(ctx.color(R.color.muted))
        isSingleLine = true
        visibility = GONE
    }

    init {
        orientation = HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setBackgroundColor(ctx.color(R.color.bar))
        setPadding(ctx.dp(if (back == null) Design.lg else Design.sm), ctx.dp(Design.sm), ctx.dp(Design.sm), ctx.dp(Design.sm))
        minimumHeight = ctx.dp(56)
        if (back != null) {
            addView(icon(R.drawable.ic_back, "Back") { back() })
        }
        val titles = LinearLayout(ctx).apply {
            orientation = VERTICAL
            addView(title)
            addView(subtitle)
        }
        addView(titles, LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = ctx.dp(if (back == null) 0 else Design.sm) })
    }

    fun icon(res: Int, label: String, onClick: (View) -> Unit) = ImageButton(context).apply {
        setImageResource(res)
        imageTintList = android.content.res.ColorStateList.valueOf(context.color(R.color.text))
        contentDescription = label
        tooltipText = label
        background = context.ripple(borderless = true)
        layoutParams = LayoutParams(context.dp(Design.touchTargetMin), context.dp(Design.touchTargetMin))
        setOnClickListener { v ->
            Design.mediumHaptic(v)
            onClick(v)
        }
    }

    fun action(res: Int, label: String, onClick: (View) -> Unit) {
        addView(icon(res, label, onClick))
    }
}

/** A circle with a contact's initial, tinted from its fingerprint. */
class Avatar(ctx: Context, size: Int = 44) : TextView(ctx) {
    init {
        gravity = Gravity.CENTER
        textSize = size / 2.6f
        setTextColor(Color.WHITE)
        setTypeface(typeface, Design.weightBold)
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

fun Context.label(
    text: String,
    size: Float = Design.typeBody,
    colorId: Int = R.color.text,
    weight: Int = Design.weightRegular,
    maxLines: Int = 1,
) = TextView(this).apply {
    this.text = text
    textSize = size
    setTextColor(color(colorId))
    setTypeface(typeface, weight)
    if (maxLines > 0) this.maxLines = maxLines
}

/** An AlertDialog.Builder whose dialogs follow screen security (see [Privacy.secure]). */
class SecureBuilder(ctx: Context) : android.app.AlertDialog.Builder(ctx) {
    override fun create(): android.app.AlertDialog = super.create().also { Privacy.secure(it) }

    /** Sets positive button with consistent styling and haptic feedback. */
    @Suppress("UNUSED_PARAMETER")
    fun setPositiveButton(text: String, onClick: (android.content.DialogInterface, Int) -> Unit = { _, _ -> }): SecureBuilder {
        return super.setPositiveButton(text) { dialog, which ->
            (dialog as? android.app.AlertDialog)?.getButton(android.app.AlertDialog.BUTTON_POSITIVE)?.let { Design.mediumHaptic(it) }
            onClick(dialog, which)
        } as SecureBuilder
    }

    /** Sets negative button with consistent styling. */
    @Suppress("UNUSED_PARAMETER")
    fun setNegativeButton(text: String, onClick: (android.content.DialogInterface, Int) -> Unit = { _, _ -> }): SecureBuilder {
        return super.setNegativeButton(text) { dialog, which ->
            onClick(dialog, which)
        } as SecureBuilder
    }

    /** Sets neutral button with consistent styling. */
    @Suppress("UNUSED_PARAMETER")
    fun setNeutralButton(text: String, onClick: (android.content.DialogInterface, Int) -> Unit = { _, _ -> }): SecureBuilder {
        return super.setNeutralButton(text) { dialog, which ->
            onClick(dialog, which)
        } as SecureBuilder
    }
}

/**
 * A tray that rises from the bottom edge, in the theme's colours, with a
 * handle; a tap outside or Back closes it. Shown with `show()`.
 */
fun Activity.bottomSheet(content: View): android.app.Dialog = android.app.Dialog(this).apply {
    requestWindowFeature(android.view.Window.FEATURE_NO_TITLE)
    val activity = this@bottomSheet
    val r = activity.dp(Design.radiusXl).toFloat()
    val frame = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        background = GradientDrawable().apply {
            setColor(activity.color(R.color.sheet))
            cornerRadii = floatArrayOf(r, r, r, r, 0f, 0f, 0f, 0f)
        }
        setPadding(activity.dp(Design.screenPadding), activity.dp(Design.md), activity.dp(Design.screenPadding), activity.dp(Design.md))
        addView(View(activity).apply {
            background = roundedRes(R.color.muted, Design.radiusSm)
            alpha = 0.4f
        }, LinearLayout.LayoutParams(activity.dp(32), activity.dp(4)).apply {
            gravity = Gravity.CENTER_HORIZONTAL
            topMargin = activity.dp(Design.xs)
            bottomMargin = activity.dp(Design.sm)
        })
        addView(content, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
    }
    val bottom = frame.paddingBottom
    frame.setOnApplyWindowInsetsListener { v, insets ->
        val nav = if (Build.VERSION.SDK_INT >= 30) insets.getInsets(WindowInsets.Type.navigationBars() or WindowInsets.Type.ime()).bottom
            else @Suppress("DEPRECATION") insets.systemWindowInsetBottom
        v.setPadding(activity.dp(Design.screenPadding), activity.dp(Design.md), activity.dp(Design.screenPadding), bottom + nav + activity.dp(Design.md))
        insets
    }
    setContentView(frame)
    setCanceledOnTouchOutside(true)
    Privacy.secure(this)
    window?.apply {
        setLayout(MATCH_PARENT, WRAP_CONTENT)
        setGravity(Gravity.BOTTOM)
        setBackgroundDrawable(android.graphics.drawable.ColorDrawable(Color.TRANSPARENT))
        setWindowAnimations(android.R.style.Animation_InputMethod)
        if (Build.VERSION.SDK_INT >= 30) setDecorFitsSystemWindows(false)
    }
}

/**
 * Lets a field grow onto more lines (up to `max`) instead of scrolling
 * sideways. With `newlines`, Enter starts a new line (messages and other
 * prose); without, text only wraps and Enter means done (names, links).
 * Call it after setting the input type.
 */
fun EditText.wrapping(newlines: Boolean = true, max: Int = 6): EditText {
    val type = inputType or InputType.TYPE_CLASS_TEXT
    isSingleLine = false
    setHorizontallyScrolling(false)
    maxLines = max
    if (newlines) {
        inputType = type or InputType.TYPE_TEXT_FLAG_MULTI_LINE
    } else {
        // Shown on several lines, typed as one: the keyboard offers Done.
        setRawInputType(type and InputType.TYPE_TEXT_FLAG_MULTI_LINE.inv())
        imeOptions = EditorInfo.IME_ACTION_DONE
    }
    return this
}

val matchWrap get() = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)

/** Primary action button: filled, accent color. */
fun Context.primaryButton(text: String, onClick: (View) -> Unit) = android.widget.Button(this).apply {
    this.text = text
    isAllCaps = false
    setTextColor(color(R.color.on_accent))
    background = roundedRes(R.color.accent, Design.radiusXl)
    minHeight = dp(Design.touchTargetMin)
    setPadding(dp(Design.lg), dp(Design.buttonPaddingV), dp(Design.lg), dp(Design.buttonPaddingV))
    setOnClickListener { v -> Design.mediumHaptic(v); onClick(v) }
}

/** Secondary action button: outlined/tinted, subtle. */
fun Context.secondaryButton(text: String, onClick: (View) -> Unit) = android.widget.Button(this).apply {
    this.text = text
    isAllCaps = false
    setTextColor(color(R.color.accent))
    background = ripple()
    minHeight = dp(Design.touchTargetMin)
    setPadding(dp(Design.lg), dp(Design.buttonPaddingV), dp(Design.lg), dp(Design.buttonPaddingV))
    setOnClickListener { v -> Design.mediumHaptic(v); onClick(v) }
}

/** Tertiary/ghost button: text only. */
fun Context.tertiaryButton(text: String, onClick: (View) -> Unit) = android.widget.TextView(this).apply {
    this.text = text
    textSize = Design.typeButton
    setTextColor(color(R.color.accent))
    setTypeface(typeface, Design.weightBold)
    gravity = Gravity.CENTER
    minHeight = dp(Design.touchTargetMin)
    setPadding(dp(Design.md), dp(Design.sm), dp(Design.md), dp(Design.sm))
    background = ripple(borderless = true)
    setOnClickListener { v -> Design.lightHaptic(v); onClick(v) }
}

/** Empty state view with illustration, message, and action. */
fun Activity.emptyState(
    title: String,
    message: String,
    actionText: String? = null,
    action: (() -> Unit)? = null,
    iconRes: Int = 0,
): LinearLayout = LinearLayout(this).apply {
    orientation = LinearLayout.VERTICAL
    gravity = Gravity.CENTER
    setPadding(dp(Design.xl), dp(Design.xxl), dp(Design.xl), dp(Design.xxl))
    val activity = this@emptyState
    if (iconRes != 0) {
        addView(android.widget.ImageView(activity).apply {
            setImageResource(iconRes)
            setColorFilter(activity.color(R.color.muted))
            layoutParams = LinearLayout.LayoutParams(dp(80), dp(80)).apply { bottomMargin = dp(Design.lg) }
        })
    }
    addView(label(title, Design.typeTitle, R.color.text, Design.weightBold).apply {
        gravity = Gravity.CENTER
        setPadding(0, 0, 0, dp(Design.sm))
    }, matchWrap)
    addView(label(message, Design.typeBody, R.color.muted).apply {
        gravity = Gravity.CENTER
        setPadding(0, 0, 0, dp(Design.lg))
    }, matchWrap)
    actionText?.let { text ->
        addView(activity.primaryButton(text) { action?.invoke() }, matchWrap.apply { topMargin = dp(Design.md) })
    }
}

fun members(n: Int) = if (n == 1) "1 member" else "$n members"
