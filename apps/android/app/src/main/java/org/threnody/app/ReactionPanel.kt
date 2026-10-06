package org.threnody.app

import android.app.Activity
import android.content.Context
import android.view.Gravity
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView

/**
 * What a long press on a message opens: reactions that toggle in place
 * (the panel stays open, so several can be set at once), any emoji from
 * the full picker, and the message's actions, which close it.
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
    private lateinit var dialog: android.app.Dialog
    private val quick = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL }
    /** Fixed while the panel is open, so nothing moves under a finger. */
    private val order: List<String> =
        ((recent(a) + DEFAULTS).distinct().take(QUICK) + mine).distinct()

    fun show() {
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(16), 0, a.dp(16), 0)
        }
        if (canReact) {
            box.addView(HorizontalScrollView(a).apply {
                isHorizontalScrollBarEnabled = false
                background = rounded(a.color(R.color.field), a.dp(28).toFloat())
                setPadding(a.dp(6), a.dp(4), a.dp(6), a.dp(4))
                addView(quick)
            }, matchWrap.apply { bottomMargin = a.dp(8) })
            drawQuick()
        }
        for ((name, run) in actions) {
            box.addView(a.label(name, 16f).apply {
                minHeight = a.dp(48)
                gravity = Gravity.CENTER_VERTICAL
                setPadding(a.dp(12), 0, a.dp(12), 0)
                background = a.ripple()
                setOnClickListener {
                    dialog.dismiss()
                    run()
                }
            }, matchWrap)
        }
        dialog = a.bottomSheet(box)
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
        // Everything else: the whole emoji set.
        quick.addView(TextView(a).apply {
            text = "＋"
            textSize = 24f
            gravity = Gravity.CENTER
            setTextColor(a.color(R.color.muted))
            background = rounded(a.color(R.color.divider), a.dp(22).toFloat())
            contentDescription = "More emoji"
            setOnClickListener {
                EmojiPicker(a, "React", stay = true, marked = { chosen }) { e -> set(e, e !in chosen) }.show()
            }
        }, LinearLayout.LayoutParams(a.dp(44), a.dp(44)).apply { marginStart = a.dp(4) })
    }

    private fun set(emoji: String, add: Boolean) {
        if (add) chosen += emoji else chosen -= emoji
        if (add) remember(a, emoji)
        toggle(emoji, add)
        drawQuick()
    }

    companion object {
        private val DEFAULTS = listOf("👍", "❤️", "😂", "😮", "😢", "🙏", "🎉", "🔥")
        private const val QUICK = 8
        private const val PREFS = "reactions"

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
