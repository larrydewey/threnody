package org.threnody.app

import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.InputType
import android.text.SpannableString
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.TextWatcher
import android.text.style.BackgroundColorSpan
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import android.widget.LinearLayout

/**
 * A search box that takes over a screen's [TopBar]: Back (the arrow or the
 * system gesture) closes it. Typing waits [DEBOUNCE_MS] before `query`
 * runs. With `stepper` (in a chat) it shows "3 of 12" and buttons to step
 * to older (+1) and newer (-1) matches through `step`.
 *
 * The keyboard is asked not to learn from what's typed: a search is often
 * for something private.
 */
class SearchBox(
    private val activity: Activity,
    private val bar: TopBar,
    hint: String,
    stepper: Boolean,
    private val query: (String) -> Unit,
    private val step: (Int) -> Unit = {},
    private val closed: () -> Unit,
) {
    // Not `handler`: inside the EditText below that would be the view's own
    // (null until it is attached to a window).
    private val debounce = Handler(Looper.getMainLooper())
    private val fire = Runnable { query(text) }
    /** The API 33+ Back callback while open (an OnBackInvokedCallback). */
    private var back: Any? = null

    var isOpen = false
        private set

    /** The query as typed, trimmed. */
    val text get() = field.text.toString().trim()

    val field: EditText = EditText(activity).apply {
        this.hint = hint
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        imeOptions = EditorInfo.IME_ACTION_SEARCH or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        isSingleLine = true
        textSize = Design.typeSubtitle
        setTextColor(activity.color(R.color.text))
        setHintTextColor(activity.color(R.color.muted))
        background = activity.roundedRes(R.color.field, Design.radiusXl)
        setPadding(activity.dp(Design.lg), activity.dp(Design.sm), activity.dp(Design.lg), activity.dp(Design.sm))
        addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
            override fun afterTextChanged(s: Editable?) {
                debounce.removeCallbacks(fire)
                if (isOpen) debounce.postDelayed(fire, DEBOUNCE_MS)
            }
        })
        setOnEditorActionListener { _, action, _ ->
            if (action != EditorInfo.IME_ACTION_SEARCH) return@setOnEditorActionListener false
            debounce.removeCallbacks(fire)
            fire.run()
            keyboard(false)
            true
        }
    }

    private val count = activity.label("", Design.typeCaption, R.color.muted).apply {
        setPadding(activity.dp(Design.sm), 0, activity.dp(Design.xs), 0)
        visibility = if (stepper) View.VISIBLE else View.GONE
    }
    private val older = bar.icon(R.drawable.ic_up, "Older match") { step(1) }
    private val newer = bar.icon(R.drawable.ic_down, "Newer match") { step(-1) }

    private val row = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(bar.icon(R.drawable.ic_back, "Close search") { close() })
        addView(field, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = activity.dp(Design.xs) })
        addView(count, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        if (stepper) {
            addView(older)
            addView(newer)
        }
        setCount(-1, 0)
    }

    fun open(typing: Boolean = true) {
        if (isOpen) return
        isOpen = true
        bar.replaceWith(row)
        if (typing) {
            field.requestFocus()
            keyboard(true)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            val cb = android.window.OnBackInvokedCallback { close() }
            activity.onBackInvokedDispatcher.registerOnBackInvokedCallback(
                android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT, cb,
            )
            back = cb
        }
    }

    fun close() {
        if (!isOpen) return
        isOpen = false
        debounce.removeCallbacks(fire)
        keyboard(false)
        field.setText("")
        setCount(-1, 0)
        bar.restore()
        if (Build.VERSION.SDK_INT >= 33) {
            (back as? android.window.OnBackInvokedCallback)?.let {
                activity.onBackInvokedDispatcher.unregisterOnBackInvokedCallback(it)
            }
            back = null
        }
        closed()
    }

    /** Shows "`index`+1 of `total`" (nothing for an empty query) and enables stepping. */
    fun setCount(index: Int, total: Int) {
        count.text = when {
            text.isEmpty() -> ""
            total == 0 -> "No matches"
            else -> "${index + 1} of $total"
        }
        older.isEnabled = index + 1 < total
        newer.isEnabled = index > 0
        older.alpha = if (older.isEnabled) 1f else 0.38f
        newer.alpha = if (newer.isEnabled) 1f else 0.38f
    }

    private fun keyboard(show: Boolean) {
        val imm = activity.getSystemService(InputMethodManager::class.java) ?: return
        if (show) field.post { imm.showSoftInput(field, InputMethodManager.SHOW_IMPLICIT) }
        else imm.hideSoftInputFromWindow(field.windowToken, 0)
    }

    companion object {
        const val DEBOUNCE_MS = 250L
    }
}

/**
 * Finding a query's terms in text, to show what matched. The node does the
 * searching; this reads the query the same way: whitespace-separated
 * terms or "quoted phrases", ignoring case.
 */
object Search {
    /** Marks the current match in a chat, over the others. */
    private val STRONG = Color.rgb(0xFF, 0xB3, 0x00)
    private val WEAK = Color.argb(0xA0, 0xFF, 0xD5, 0x4F)

    fun terms(query: String): List<String> {
        val terms = mutableListOf<String>()
        var rest = query
        while (true) {
            val start = rest.indexOfFirst { !it.isWhitespace() }
            if (start < 0) break
            rest = rest.substring(start)
            val term: String
            if (rest.startsWith('"')) {
                val end = rest.indexOf('"', 1)
                term = if (end < 0) rest.substring(1) else rest.substring(1, end)
                rest = if (end < 0) "" else rest.substring(end + 1)
            } else {
                val end = rest.indexOfFirst { it.isWhitespace() }.let { if (it < 0) rest.length else it }
                term = rest.substring(0, end)
                rest = rest.substring(end)
            }
            val t = term.trim().lowercase()
            if (t.isNotEmpty() && t !in terms) terms.add(t)
        }
        return terms
    }

    /** Where any of `terms` occurs in `text`, in order, overlaps merged. */
    fun ranges(text: CharSequence, terms: List<String>): List<IntRange> {
        val found = terms.flatMap { t ->
            generateSequence(text.indexOf(t, 0, ignoreCase = true).takeIf { it >= 0 }) { at ->
                text.indexOf(t, at + t.length, ignoreCase = true).takeIf { it >= 0 }
            }.map { it until it + t.length }.toList()
        }.sortedBy { it.first }
        val merged = mutableListOf<IntRange>()
        for (r in found) {
            val last = merged.lastOrNull()
            if (last != null && r.first <= last.last + 1) merged[merged.size - 1] = last.first..maxOf(last.last, r.last)
            else merged.add(r)
        }
        return merged
    }

    /** `text` with matches marked: brighter for the current match in a chat. */
    fun highlight(text: CharSequence, terms: List<String>, strong: Boolean): CharSequence {
        val found = ranges(text, terms)
        if (found.isEmpty()) return text
        val s = SpannableString(text)
        for (r in found) {
            s.setSpan(BackgroundColorSpan(if (strong) STRONG else WEAK), r.first, r.last + 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            // Dark on the yellow, also in a coloured outgoing bubble.
            s.setSpan(ForegroundColorSpan(Color.BLACK), r.first, r.last + 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        return s
    }

    /**
     * A line of `text` around its first match (cut at about `around`
     * characters each side), with the matches in bold and tinted.
     */
    fun snippet(text: String, terms: List<String>, tint: Int, around: Int = 32): CharSequence {
        val flat = text.replace('\n', ' ')
        val first = ranges(flat, terms).firstOrNull()
        val from = if (first == null || first.first <= around) 0
            else flat.lastIndexOf(' ', first.first - around).let { if (it < 0) first.first - around else it + 1 }
        val until = minOf(flat.length, (first?.last ?: 0) + around * 2)
        val cut = (if (from > 0) "…" else "") + flat.substring(from, until) + (if (until < flat.length) "…" else "")
        val s = SpannableStringBuilder(cut)
        for (r in ranges(cut, terms)) {
            s.setSpan(StyleSpan(Typeface.BOLD), r.first, r.last + 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            s.setSpan(ForegroundColorSpan(tint), r.first, r.last + 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        return s
    }

    /** Whether every term is in `text` (for matching conversation titles). */
    fun all(text: String, terms: List<String>) = terms.isNotEmpty() && terms.all { text.contains(it, ignoreCase = true) }
}
