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
import android.widget.TextView
import android.widget.Toast
import uniffi.threnody_ffi.TextLink

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
}
