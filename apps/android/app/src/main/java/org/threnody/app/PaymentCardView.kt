package org.threnody.app

import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import uniffi.threnody_ffi.cardBrand

/**
 * A payment card drawn as one: the ID-1 shape (85.6 × 54 mm), its
 * network's colours, a chip, the number in groups, the holder and the
 * expiry. What it shows is up to the caller (the number covered but for
 * its last four digits, until revealed).
 */
class PaymentCardView(ctx: Context) : FrameLayout(ctx) {
    private val title = text(14f, bold = true)
    private val brand = text(16f, bold = true).apply {
        setTypeface(Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD_ITALIC))
        gravity = Gravity.END
    }
    private val number = text(19f).apply {
        typeface = Typeface.MONOSPACE
        letterSpacing = 0.08f
        setShadowLayer(2f, 0f, 1f, 0x55000000)
    }
    private val holder = text(13f, bold = true)
    private val expiry = text(13f, bold = true).apply { gravity = Gravity.END }
    private val corner = ctx.dp(16).toFloat()

    init {
        clipToOutline = true
        elevation = ctx.dp(2).toFloat()
        val pad = ctx.dp(18)
        val column = LinearLayout(ctx).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, ctx.dp(14), pad, ctx.dp(14))
        }
        column.addView(LinearLayout(ctx).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(title, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(brand, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        })
        column.addView(View(ctx), LinearLayout.LayoutParams(1, 0, 1f))
        // The chip.
        column.addView(View(ctx).apply {
            background = GradientDrawable(GradientDrawable.Orientation.TL_BR, intArrayOf(0xFFE9D38C.toInt(), 0xFFB8963E.toInt()))
                .apply { cornerRadius = ctx.dp(5).toFloat() }
        }, LinearLayout.LayoutParams(ctx.dp(36), ctx.dp(27)).apply { bottomMargin = ctx.dp(10) })
        column.addView(number)
        column.addView(View(ctx), LinearLayout.LayoutParams(1, 0, 1f))
        fun caption(t: String, end: Boolean = false) = text(9f).apply {
            text = t
            alpha = 0.75f
            letterSpacing = 0.12f
            if (end) gravity = Gravity.END
        }
        column.addView(LinearLayout(ctx).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(LinearLayout(ctx).apply {
                orientation = LinearLayout.VERTICAL
                addView(caption("CARDHOLDER"))
                addView(holder)
            }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(LinearLayout(ctx).apply {
                orientation = LinearLayout.VERTICAL
                addView(caption("EXPIRES", end = true), LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(expiry, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        })
        addView(column, LayoutParams(MATCH_PARENT, MATCH_PARENT))
        show("", "", "", "", "")
    }

    private fun text(size: Float, bold: Boolean = false) = TextView(context).apply {
        textSize = size
        setTextColor(Color.WHITE)
        if (bold) setTypeface(typeface, Typeface.BOLD)
        maxLines = 1
        ellipsize = android.text.TextUtils.TruncateAt.END
    }

    /** Draws the card; `digits` decides its network and colours, `shownNumber` is what the number line says. */
    fun show(cardTitle: String, digits: String, shownNumber: String, cardHolder: String, expires: String) {
        val b = cardBrand(digits)
        title.text = cardTitle
        brand.text = BRANDS[b] ?: ""
        number.text = shownNumber.ifEmpty { "•••• •••• •••• ••••" }
        holder.text = cardHolder.uppercase().ifEmpty { "—" }
        expiry.text = expires.ifEmpty { "MM/YY" }
        val (from, to) = COLOURS[b] ?: (0xFF334155.toInt() to 0xFF64748B.toInt())
        background = GradientDrawable(GradientDrawable.Orientation.TL_BR, intArrayOf(from, to)).apply { cornerRadius = corner }
        contentDescription = listOf(BRANDS[b] ?: "Payment card", cardTitle, "ending ${digits.filter { it.isDigit() }.takeLast(4)}")
            .filter { it.isNotBlank() }.joinToString(", ")
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val w = MeasureSpec.getSize(widthMeasureSpec)
        super.onMeasure(
            MeasureSpec.makeMeasureSpec(w, MeasureSpec.EXACTLY),
            MeasureSpec.makeMeasureSpec((w / 1.586f).toInt(), MeasureSpec.EXACTLY),
        )
    }

    companion object {
        val BRANDS = mapOf(
            "visa" to "VISA", "mastercard" to "mastercard", "amex" to "AMEX", "discover" to "DISCOVER",
            "diners" to "Diners Club", "jcb" to "JCB", "unionpay" to "UnionPay",
        )
        private val COLOURS = mapOf(
            "visa" to (0xFF1A1F71.toInt() to 0xFF3949C8.toInt()),
            "mastercard" to (0xFF1F1F1F.toInt() to 0xFFB4410E.toInt()),
            "amex" to (0xFF0B4F7C.toInt() to 0xFF2E9BD6.toInt()),
            "discover" to (0xFF2B1A10.toInt() to 0xFFD9731C.toInt()),
            "diners" to (0xFF0F3D5E.toInt() to 0xFF5B8DB8.toInt()),
            "jcb" to (0xFF0E4D2E.toInt() to 0xFF2F9E5E.toInt()),
            "unionpay" to (0xFF7A0E1C.toInt() to 0xFF1C5D8C.toInt()),
        )

        /** "•••• 1111" covered, or the number in groups. */
        fun shownNumber(digits: String, revealed: Boolean): String {
            val d = digits.filter { it.isDigit() }
            if (revealed || d.isEmpty()) return uniffi.threnody_ffi.formatCardNumber(d)
            return "•••• •••• •••• " + d.takeLast(4)
        }
    }
}
