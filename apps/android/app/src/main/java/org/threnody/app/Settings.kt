package org.threnody.app

import android.app.Activity
import android.app.Dialog
import android.graphics.drawable.ColorDrawable
import android.os.Build
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView

/**
 * A full-screen settings page: named sections of switches and links.
 *
 * Each switch says, under its name, what its current state means (what the
 * main menu used to say in a toast after the change), so the choice is
 * explained before it is made. The page reads every state again whenever
 * it gets focus back, since some changes finish elsewhere (a permission
 * prompt, a confirmation dialog).
 */
class SettingsPage(private val activity: Activity, private val title: String) {
    private val rows = mutableListOf<() -> Unit>()
    private val body = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(0, 0, 0, activity.dp(Design.xl))
    }

    /** Starts a new section called `name`. */
    fun section(name: String) {
        val first = body.childCount == 0
        body.addView(activity.label(name, Design.typeBodySmall, R.color.accent, Design.weightBold).apply {
            setPadding(
                activity.dp(Design.listItemPaddingH), activity.dp(if (first) Design.md else Design.xl),
                activity.dp(Design.listItemPaddingH), activity.dp(Design.xs),
            )
        }, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
    }

    /**
     * A switch called `name`, on when `on()` says so; `describe` says what
     * the given state means. A tap calls `set` with the new state.
     */
    fun toggle(name: String, on: () -> Boolean, describe: (Boolean) -> String, set: (Boolean) -> Unit) {
        val sw = Switch(activity).apply {
            isClickable = false
            isFocusable = false
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        val (row, sub) = row(name, "", trailing = sw)
        val bind = {
            val now = on()
            sw.isChecked = now
            sub.text = describe(now)
            row.contentDescription = "$name, ${if (now) "on" else "off"}. ${describe(now)}"
        }
        row.setOnClickListener { v ->
            Design.lightHaptic(v)
            set(!on())
            refresh()
        }
        rows += bind
        bind()
    }

    /** A row that does something when tapped: opens a screen, asks, erases. */
    fun link(name: String, subtitle: String? = null, danger: Boolean = false, onClick: () -> Unit) {
        val (row, _) = row(name, subtitle, danger = danger)
        row.setOnClickListener { v ->
            Design.lightHaptic(v)
            onClick()
        }
    }

    private fun row(
        name: String,
        subtitle: String?,
        trailing: View? = null,
        danger: Boolean = false,
    ): Pair<LinearLayout, TextView> {
        val texts = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
        }
        texts.addView(activity.label(name, Design.typeBody, if (danger) R.color.md_sys_color_error else R.color.text, maxLines = 2))
        val sub = activity.label(subtitle ?: "", Design.typeBodySmall, R.color.muted, maxLines = 4).apply {
            visibility = if (subtitle == null) View.GONE else View.VISIBLE
        }
        texts.addView(sub)
        val row = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = activity.dp(Design.touchTargetMin)
            background = activity.ripple()
            isClickable = true
            isFocusable = true
            setPadding(
                activity.dp(Design.listItemPaddingH), activity.dp(Design.listItemPaddingV),
                activity.dp(Design.listItemPaddingH), activity.dp(Design.listItemPaddingV),
            )
            addView(texts, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            if (trailing != null) {
                addView(trailing, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply {
                    marginStart = activity.dp(Design.md)
                })
            }
        }
        body.addView(row, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        return row to sub
    }

    /** Reads every switch's state again. */
    fun refresh() = rows.forEach { it() }

    /** Shows the page over the whole screen; Back or the arrow closes it. */
    fun show(): Dialog {
        val page = this
        val dialog = object : Dialog(activity) {
            override fun onWindowFocusChanged(hasFocus: Boolean) {
                super.onWindowFocusChanged(hasFocus)
                if (hasFocus) page.refresh()
            }
        }
        val root = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(activity.color(R.color.background))
        }
        val bar = TopBar(activity) { dialog.dismiss() }.apply { this.title.text = page.title }
        val scroll = ScrollView(activity).apply { addView(body) }
        root.addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        dialog.requestWindowFeature(android.view.Window.FEATURE_NO_TITLE)
        dialog.setContentView(root)
        Privacy.secure(dialog)
        dialog.window?.apply {
            setLayout(MATCH_PARENT, MATCH_PARENT)
            setBackgroundDrawable(ColorDrawable(activity.color(R.color.background)))
            if (Build.VERSION.SDK_INT >= 30) {
                setDecorFitsSystemWindows(false)
                val top = bar.paddingTop
                val bottom = body.paddingBottom
                root.setOnApplyWindowInsetsListener { _, insets ->
                    val bars = insets.getInsets(
                        android.view.WindowInsets.Type.systemBars() or android.view.WindowInsets.Type.displayCutout(),
                    )
                    root.setPadding(bars.left, 0, bars.right, 0)
                    bar.setPadding(bar.paddingLeft, top + bars.top, bar.paddingRight, bar.paddingBottom)
                    body.setPadding(body.paddingLeft, body.paddingTop, body.paddingRight, bottom + bars.bottom)
                    android.view.WindowInsets.CONSUMED
                }
            }
        }
        dialog.show()
        return dialog
    }
}
