package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.Gravity
import android.view.WindowManager
import android.widget.EditText
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import java.text.BreakIterator

/**
 * What a long press on a message opens: reactions that toggle in place
 * (the panel stays open, so several can be set at once), any emoji from
 * the keyboard, and the message's actions, which close it.
 */
class ReactionPanel(
    private val a: Activity,
    /** Our reactions on the message now. */
    mine: Set<String>,
    private val canReact: Boolean,
    private val actions: List<Pair<String, () -> Unit>>,
    /** Adds (true) or takes away (false) our emoji. */
    private val toggle: (String, Boolean) -> Unit,
) {
    private val chosen = mine.toMutableSet()
    private lateinit var dialog: AlertDialog
    private val quick = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL }
    /** Fixed while the panel is open, so nothing moves under a finger. */
    private val order: List<String> =
        ((recent(a) + DEFAULTS).distinct().take(QUICK) + mine).distinct()

    fun show() {
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(12), a.dp(12), a.dp(12), a.dp(4))
        }
        if (canReact) {
            box.addView(HorizontalScrollView(a).apply {
                isHorizontalScrollBarEnabled = false
                addView(quick)
            }, matchWrap)
            drawQuick()
            box.addView(keyboardField(), matchWrap)
        }
        for ((name, run) in actions) {
            box.addView(a.label(name, 16f).apply {
                minHeight = a.dp(48)
                gravity = Gravity.CENTER_VERTICAL
                setPadding(a.dp(8), 0, a.dp(8), 0)
                background = a.getDrawable(android.R.drawable.list_selector_background)
                setOnClickListener {
                    dialog.dismiss()
                    run()
                }
            }, matchWrap)
        }
        dialog = AlertDialog.Builder(a)
            .setView(box)
            .setPositiveButton("Done", null)
            .create()
        dialog.setCanceledOnTouchOutside(true)
        // The keyboard comes up only when the field is tapped.
        dialog.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_HIDDEN or
            WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
        dialog.show()
    }

    /**
     * Recent reactions first, then the usual ones, then ours that aren't
     * among them; ones added from the keyboard join the end.
     */
    private fun drawQuick() {
        quick.removeAllViews()
        for (emoji in order + chosen.filter { it !in order }) {
            val on = emoji in chosen
            quick.addView(TextView(a).apply {
                text = emoji
                textSize = 26f
                gravity = Gravity.CENTER
                contentDescription = if (on) "$emoji, yours: tap to take back" else "React $emoji"
                background = if (on) rounded(a.color(R.color.divider), a.dp(22).toFloat()) else null
                setOnClickListener { set(emoji, !on) }
            }, LinearLayout.LayoutParams(a.dp(48), a.dp(48)).apply { marginEnd = a.dp(2) })
        }
    }

    private fun set(emoji: String, add: Boolean) {
        if (add) chosen += emoji else chosen -= emoji
        if (add) remember(a, emoji)
        toggle(emoji, add)
        drawQuick()
    }

    /** Typing (or picking from the keyboard's emoji) adds each emoji at once. */
    private fun keyboardField() = EditText(a).apply {
        hint = "Add from your keyboard 😀"
        textSize = 18f
        isSingleLine = true
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
            override fun afterTextChanged(s: Editable) {
                val emoji = emojiIn(s.toString())
                if (emoji.isEmpty()) {
                    // Letters aren't reactions; leave them for a moment so it's clear why.
                    if (s.length > 12) s.clear()
                    return
                }
                s.clear()
                for (e in emoji) if (e !in chosen) set(e, true)
            }
        })
    }

    companion object {
        private val DEFAULTS = listOf("👍", "❤️", "😂", "😮", "😢", "🙏", "🎉", "🔥")
        private const val QUICK = 8
        private const val PREFS = "reactions"

        /** The emoji (whole grapheme clusters, with skin tones and joins) in `text`. */
        fun emojiIn(text: String): List<String> {
            val it = BreakIterator.getCharacterInstance()
            it.setText(text)
            val out = mutableListOf<String>()
            var start = it.first()
            var end = it.next()
            while (end != BreakIterator.DONE) {
                val g = text.substring(start, end)
                if (isEmoji(g)) out += g
                start = end
                end = it.next()
            }
            return out
        }

        private fun isEmoji(g: String): Boolean {
            val cp = g.codePointAt(0)
            if (Character.isLetterOrDigit(cp) || Character.isWhitespace(cp)) return false
            return Character.getType(cp) == Character.OTHER_SYMBOL.toInt() ||
                cp in 0x1F000..0x1FAFF || cp in 0x2600..0x27BF || g.contains('️')
        }

        private fun recent(ctx: Context): List<String> =
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString("recent", "")!!
                .split('\n').filter { it.isNotEmpty() }

        private fun remember(ctx: Context, emoji: String) {
            val list = (listOf(emoji) + recent(ctx)).distinct().take(QUICK)
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
                .putString("recent", list.joinToString("\n")).apply()
        }
    }
}
