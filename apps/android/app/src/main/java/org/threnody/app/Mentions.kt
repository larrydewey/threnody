package org.threnody.app

import android.graphics.Typeface
import android.text.Editable
import android.text.SpannableString
import android.text.Spanned
import android.text.TextWatcher
import android.text.style.BackgroundColorSpan
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.widget.ArrayAdapter
import android.widget.EditText
import android.widget.ListPopupWindow
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.MentionInfo

/**
 * @mentions in a group's compose field: typing "@" offers the members,
 * and picking one puts "@Name" in, marked with a [Tag] that remembers
 * the member. The node counts characters (code points), Android UTF-16
 * units, so offsets are converted both ways here.
 */
object Mentions {
    /** Marks "@Name" in the compose field as mentioning `device`. */
    class Tag(val device: String, val label: String)

    /**
     * Watches `field` for "@" followed by a name's start, and offers the
     * matching entries of `members()` (device to name) in a popup.
     */
    fun attach(field: EditText, members: () -> List<Pair<String, String>>) {
        val popup = ListPopupWindow(field.context).apply {
            anchorView = field
            isModal = false
            width = (260 * field.resources.displayMetrics.density).toInt()
        }
        field.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
            override fun afterTextChanged(s: Editable) {
                // An edited mention is just text again.
                for (t in s.getSpans(0, s.length, Tag::class.java)) {
                    val a = s.getSpanStart(t)
                    val b = s.getSpanEnd(t)
                    if (a < 0 || s.subSequence(a, b).toString() != t.label) s.removeSpan(t)
                }
                val cursor = field.selectionStart
                if (cursor < 0) return popup.dismiss()
                val at = s.lastIndexOf('@', cursor - 1)
                val query = if (at >= 0 && (at == 0 || s[at - 1].isWhitespace())) s.substring(at + 1, cursor) else null
                if (query == null || query.any { it.isWhitespace() } || s.getSpans(at, at + 1, Tag::class.java).isNotEmpty()) {
                    return popup.dismiss()
                }
                val hits = members().filter { (_, name) -> name.startsWith(query, ignoreCase = true) }.take(6)
                if (hits.isEmpty()) return popup.dismiss()
                popup.setAdapter(ArrayAdapter(field.context, android.R.layout.simple_list_item_1, hits.map { it.second }))
                popup.setOnItemClickListener { _, _, i, _ ->
                    val (device, name) = hits[i]
                    val label = "@$name"
                    val end = field.selectionStart.coerceAtLeast(at + 1)
                    s.replace(at, end, "$label ")
                    s.setSpan(Tag(device, label), at, at + label.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
                    popup.dismiss()
                }
                popup.show()
            }
        })
    }

    /** The mentions in `text` (trimmed by `trimStart` chars at the front), as the node counts. */
    fun of(text: Spanned, trimStart: Int, trimmed: String): List<MentionInfo> =
        text.getSpans(0, text.length, Tag::class.java)
            .map { Triple(text.getSpanStart(it) - trimStart, text.getSpanEnd(it) - trimStart, it) }
            .filter { (a, b, _) -> a >= 0 && b <= trimmed.length }
            .sortedBy { it.first }
            .map { (a, b, t) ->
                MentionInfo(
                    start = trimmed.codePointCount(0, a).toUInt(),
                    len = trimmed.codePointCount(a, b).toUInt(),
                    device = t.device,
                    mine = false,
                )
            }

    /**
     * `e`'s text with each mention shown as "@" and our name for them, and
     * where they are (UTF-16), and whether each is us.
     */
    fun display(e: HistoryEntry, name: (String) -> String): Pair<String, List<Triple<Int, Int, Boolean>>> {
        if (e.mentions.isEmpty()) return e.text to emptyList()
        val text = e.text
        val out = StringBuilder()
        val ranges = mutableListOf<Triple<Int, Int, Boolean>>()
        var from = 0
        for (m in e.mentions) {
            val a = try { text.offsetByCodePoints(0, m.start.toInt()) } catch (_: Exception) { break }
            val b = try { text.offsetByCodePoints(a, m.len.toInt()) } catch (_: Exception) { break }
            if (a < from) continue
            out.append(text, from, a)
            val label = "@" + name(m.device)
            ranges.add(Triple(out.length, out.length + label.length, m.mine))
            out.append(label)
            from = b
        }
        out.append(text, from, text.length)
        return out.toString() to ranges
    }

    /** Styles mentions in `text`: bold in the accent color, ours highlighted. */
    fun style(text: CharSequence, ranges: List<Triple<Int, Int, Boolean>>, accent: Int): CharSequence {
        if (ranges.isEmpty()) return text
        val s = SpannableString(text)
        for ((a, b, mine) in ranges) {
            if (b > s.length) continue
            s.setSpan(StyleSpan(Typeface.BOLD), a, b, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            s.setSpan(ForegroundColorSpan(accent), a, b, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            if (mine) s.setSpan(BackgroundColorSpan(accent and 0x33FFFFFF), a, b, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        return s
    }
}
