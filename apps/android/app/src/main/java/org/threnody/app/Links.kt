package org.threnody.app

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.SystemClock
import android.text.SpannableString
import android.text.Spanned
import android.text.TextPaint
import android.text.method.LinkMovementMethod
import android.text.style.ClickableSpan
import android.view.View
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Typeface
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import uniffi.threnody_ffi.LinkPreviewInfo
import uniffi.threnody_ffi.TextLink
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Web links in message text: found in Rust (`textLinks`, as on every
 * platform), tappable, and confirmed before they leave the app.
 */
object Links {
    /** When a bubble was last long-pressed: the lift that ends it isn't a tap on a link. */
    private var longPressedAt = 0L

    /** Call from a bubble's long-press handler. */
    fun longPressed() {
        longPressedAt = SystemClock.uptimeMillis()
    }

    /** The links in `text`. */
    fun of(text: String): List<TextLink> = uniffi.threnody_ffi.textLinks(text)

    /**
     * `styled` (whose characters are `raw`'s) with `links` tappable; each
     * asks before opening. Links keep the text's color and are underlined.
     */
    fun clickable(a: Activity, styled: CharSequence, links: List<TextLink>): CharSequence {
        if (links.isEmpty()) return styled
        val out = SpannableString(styled)
        for (l in links) {
            val start = l.start.toInt()
            val end = start + l.len.toInt()
            if (end > out.length) continue
            out.setSpan(object : ClickableSpan() {
                override fun onClick(widget: View) {
                    if (SystemClock.uptimeMillis() - longPressedAt < 1000) return
                    confirm(a, l.url, l.host)
                }

                override fun updateDrawState(ds: TextPaint) {
                    ds.isUnderlineText = true
                }
            }, start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        return out
    }

    /** Lets a text view's links take taps. */
    fun enable(v: TextView) {
        v.movementMethod = LinkMovementMethod.getInstance()
        // The movement method makes it focusable; the bubble shouldn't be.
        v.isFocusable = false
    }

    /** Links leave the app (and show your address to that site), so ask first. */
    fun confirm(a: Activity, url: String, host: String) {
        SecureBuilder(a)
            .setTitle("Open this link?")
            .setMessage("Goes to $host\n\n$url\n\nIt opens in your browser, outside Threnody. The site sees your network address.")
            .setPositiveButton("Open") { _, _ ->
                try {
                    a.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).addCategory(Intent.CATEGORY_BROWSABLE))
                } catch (_: Exception) {
                    Toast.makeText(a, "No app can open this link", Toast.LENGTH_SHORT).show()
                }
            }
            .setNeutralButton("Copy") { _, _ ->
                a.getSystemService(android.content.ClipboardManager::class.java)
                    ?.setPrimaryClip(android.content.ClipData.newPlainText("link", url))
                Toast.makeText(a, "Copied", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Where `url` really goes, in ASCII (as the confirm dialog says). */
    fun host(url: String): String = of(url).firstOrNull()?.host ?: Uri.parse(url).host.orEmpty()

    /**
     * A message's link card: its picture, title, description and site;
     * tapping it asks before opening the link, as the link itself does.
     */
    fun card(a: Activity, p: LinkPreviewInfo, fg: Int): View = LinearLayout(a).apply {
        orientation = LinearLayout.VERTICAL
        background = rounded(androidx.core.graphics.ColorUtils.setAlphaComponent(fg, 0x22), a.dp(8).toFloat())
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = a.dp(6) }
        clipToOutline = true
        thumbnail(p.image)?.let { bmp ->
            addView(ImageView(a).apply {
                setImageBitmap(bmp)
                scaleType = ImageView.ScaleType.CENTER_CROP
                adjustViewBounds = true
                maxHeight = a.dp(180)
                contentDescription = null
            }, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        val text = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(10), a.dp(6), a.dp(10), a.dp(8))
        }
        fun line(t: String, size: Float, bold: Boolean, lines: Int, alpha: Float) {
            if (t.isEmpty()) return
            text.addView(TextView(a).apply {
                this.text = t
                textSize = size
                setTextColor(fg)
                this.alpha = alpha
                maxLines = lines
                ellipsize = android.text.TextUtils.TruncateAt.END
                if (bold) setTypeface(typeface, Typeface.BOLD)
            })
        }
        line(p.title, 14f, true, 2, 1f)
        line(p.description, 13f, false, 3, 0.85f)
        line(p.site.ifEmpty { host(p.url) }, 12f, false, 1, 0.7f)
        addView(text, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        contentDescription = "Link preview: ${p.title.ifEmpty { host(p.url) }}"
        // Its texts take taps too: the bubble gives them its long press,
        // which would otherwise keep a tap from reaching the card.
        val open = View.OnClickListener {
            if (SystemClock.uptimeMillis() - longPressedAt >= 1000) confirm(a, p.url, host(p.url))
        }
        setOnClickListener(open)
        for (i in 0 until text.childCount) text.getChildAt(i).setOnClickListener(open)
        getChildAt(0)?.takeIf { it is ImageView }?.setOnClickListener(open)
    }

    private fun thumbnail(jpeg: ByteArray): Bitmap? =
        if (jpeg.isEmpty()) null else try { BitmapFactory.decodeByteArray(jpeg, 0, jpeg.size) } catch (_: Exception) { null }

    /**
     * A fetched picture as a card's thumbnail: at most 400 px on its
     * longer side, a JPEG within the size a message carries. Empty if it
     * can't be made that small or isn't a picture.
     */
    fun shrink(raw: ByteArray): ByteArray {
        if (raw.isEmpty()) return raw
        return try {
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(raw, 0, raw.size, bounds)
            if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return ByteArray(0)
            var sample = 1
            while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= 400) sample *= 2
            val decoded = BitmapFactory.decodeByteArray(raw, 0, raw.size,
                BitmapFactory.Options().apply { inSampleSize = sample }) ?: return ByteArray(0)
            val scale = minOf(1f, 400f / maxOf(decoded.width, decoded.height))
            val bmp = Bitmap.createScaledBitmap(decoded, maxOf(1, (decoded.width * scale).toInt()),
                maxOf(1, (decoded.height * scale).toInt()), true)
            val limit = uniffi.threnody_ffi.maxPreviewImage().toInt()
            for (quality in listOf(80, 65, 50, 35)) {
                val out = java.io.ByteArrayOutputStream()
                bmp.compress(Bitmap.CompressFormat.JPEG, quality, out)
                if (out.size() <= limit) return out.toByteArray()
            }
            ByteArray(0)
        } catch (_: Exception) {
            ByteArray(0)
        }
    }
}

/**
 * Above the compose field: offers a card for the first link in what's
 * being written. Nothing is fetched until the user taps the offer: the
 * fetch goes from this device, so the site sees its address. The card
 * then shows with a button to drop it, and goes with the message.
 */
class PreviewBar(private val a: Activity, private val node: () -> ThrenodyNode?) {
    val view = LinearLayout(a).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setBackgroundColor(a.getColor(R.color.bar))
        setPadding(a.dp(12), a.dp(4), a.dp(4), a.dp(4))
        visibility = View.GONE
    }
    /** The link on offer or shown, and its card once fetched. */
    private var url: String? = null
    private var card: LinkPreviewInfo? = null
    private var fetching = false
    /** Links the user dropped or that had nothing to show: not offered again. */
    private val dropped = mutableSetOf<String>()

    /** Follows the compose field. */
    fun onText(text: String) {
        val first = Links.of(text).map { it.url }.firstOrNull { it !in dropped }
        if (first == url) return
        url = first
        card = null
        fetching = false
        show()
    }

    /** The card to send with `text` (if its link is still there), and resets. */
    fun take(text: String): LinkPreviewInfo? {
        val c = card?.takeIf { c -> Links.of(text).any { it.url == c.url } }
        url = null
        card = null
        fetching = false
        dropped.clear()
        show()
        return c
    }

    private fun show() {
        view.removeAllViews()
        val u = url
        if (u == null) {
            view.visibility = View.GONE
            return
        }
        val c = card
        val label = when {
            c != null -> c.title.ifEmpty { c.description }.ifEmpty { Links.host(u) }
            fetching -> "Getting a preview of ${Links.host(u)}…"
            else -> "Add a preview of ${Links.host(u)}"
        }
        view.addView(TextView(a).apply {
            text = if (c != null) "🔗 $label" else label
            textSize = 14f
            setTextColor(a.getColor(if (c != null) R.color.text else R.color.accent))
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
            if (c == null && !fetching) {
                contentDescription = "$label. The site sees your network address."
                setOnClickListener { fetch(u) }
            }
        }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        view.addView(ImageButton(a).apply {
            setImageResource(R.drawable.ic_close)
            imageTintList = android.content.res.ColorStateList.valueOf(a.getColor(R.color.muted))
            contentDescription = if (c != null) "Remove the preview" else "Don't add a preview"
            background = null
            setOnClickListener { drop(u) }
        }, LinearLayout.LayoutParams(a.dp(40), a.dp(40)))
        view.visibility = View.VISIBLE
    }

    private fun drop(u: String) {
        dropped.add(u)
        url = null
        card = null
        fetching = false
        show()
    }

    private fun fetch(u: String) {
        val n = node() ?: return
        fetching = true
        show()
        Threading.io {
            val got = try { n.fetchLinkPreview(u) } catch (_: Exception) { null }
            val ready = got?.let { it.copy(image = Links.shrink(it.image)) }
            a.runOnUiThread {
                if (url != u) return@runOnUiThread
                fetching = false
                if (ready == null) {
                    Toast.makeText(a, "No preview for ${Links.host(u)}", Toast.LENGTH_SHORT).show()
                    drop(u)
                } else {
                    card = ready
                    show()
                }
            }
        }
    }
}
