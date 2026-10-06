package org.threnody.app

import android.app.Activity
import android.content.Context
import android.graphics.Paint
import android.text.Editable
import android.text.TextWatcher
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.BaseAdapter
import android.widget.EditText
import android.widget.AbsListView
import android.widget.ListView
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.TextView

/** Every emoji this device can draw, by category (from assets/emoji.txt, Unicode 17). */
object EmojiData {
    class Emoji(val glyph: String, val name: String, val tones: List<String>)
    class Group(val name: String, val icon: String, val emoji: List<Emoji>)

    @Volatile private var loaded: List<Group>? = null

    private val ICONS = mapOf(
        "Smileys & Emotion" to "😀", "People & Body" to "👋", "Animals & Nature" to "🐻",
        "Food & Drink" to "🍔", "Travel & Places" to "✈️", "Activities" to "⚽",
        "Objects" to "💡", "Symbols" to "🔣", "Flags" to "🏁",
    )

    fun groups(ctx: Context): List<Group> = loaded ?: synchronized(this) {
        loaded ?: read(ctx).also { loaded = it }
    }

    private fun read(ctx: Context): List<Group> {
        // Newer emoji than the system font knows would show as boxes.
        val paint = Paint()
        val out = mutableListOf<Group>()
        var name = ""
        var list = mutableListOf<Emoji>()
        fun close() {
            if (name.isNotEmpty() && list.isNotEmpty()) out += Group(name, ICONS[name] ?: list.first().glyph, list)
        }
        ctx.assets.open("emoji.txt").bufferedReader().useLines { lines ->
            for (line in lines) {
                if (line.startsWith("@")) {
                    close()
                    name = line.substring(1)
                    list = mutableListOf()
                    continue
                }
                val parts = line.split('\t')
                if (parts.size < 2 || !paint.hasGlyph(parts[0])) continue
                val tones = parts.getOrNull(2)?.split(' ')?.filter { it.isNotEmpty() && paint.hasGlyph(it) }.orEmpty()
                list += Emoji(parts[0], parts[1], tones)
            }
        }
        close()
        return out
    }
}

/**
 * The whole emoji set in a tray from the bottom of the screen: recent
 * ones, then every category in one scrolling list (the tabs jump to
 * each), search by name, and skin tones on a long press. `pick` gets each
 * emoji tapped; with `stay`, the tray stays open for more (reactions),
 * else it closes. `marked` shows which emoji are already chosen.
 */
class EmojiPicker(
    private val a: Activity,
    private val title: String,
    private val stay: Boolean = false,
    private val marked: () -> Set<String> = { emptySet() },
    private val pick: (String) -> Unit,
) {
    /** A list row: a section's title, or a row of emoji. */
    private sealed class Row
    private class Header(val title: String) : Row()
    private class Cells(val emoji: List<EmojiData.Emoji>) : Row()

    private lateinit var dialog: android.app.Dialog
    private val list = ListView(a)
    private val tabs = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL }
    private var rows: List<Row> = emptyList()
    /** The row each section starts at, by tab. */
    private var starts: List<Int> = emptyList()
    private var columns = 8
    private var searching = false

    private val adapter = object : BaseAdapter() {
        override fun getCount() = rows.size
        override fun getItem(i: Int) = rows[i]
        override fun getItemId(i: Int) = i.toLong()
        override fun getViewTypeCount() = 2
        override fun getItemViewType(i: Int) = if (rows[i] is Header) 0 else 1
        override fun isEnabled(i: Int) = false
        override fun getView(i: Int, convert: View?, parent: ViewGroup?): View = when (val r = rows[i]) {
            is Header -> ((convert as? TextView) ?: TextView(a).apply {
                textSize = 13f
                setTypeface(typeface, android.graphics.Typeface.BOLD)
                setTextColor(a.color(R.color.muted))
                setPadding(a.dp(8), a.dp(12), a.dp(8), a.dp(4))
            }).apply { text = r.title }
            is Cells -> ((convert as? LinearLayout) ?: LinearLayout(a).apply {
                orientation = LinearLayout.HORIZONTAL
                repeat(columns) { addView(cell(), LinearLayout.LayoutParams(0, a.dp(50), 1f)) }
            }).apply {
                for (j in 0 until columns) bind(getChildAt(j) as TextView, r.emoji.getOrNull(j))
            }
        }
    }

    private fun cell() = TextView(a).apply {
        textSize = 28f
        gravity = Gravity.CENTER
        setOnClickListener { (tag as? EmojiData.Emoji)?.let { chose(toned(a, it)) } }
        setOnLongClickListener { v ->
            val e = tag as? EmojiData.Emoji
            if (e == null || e.tones.isEmpty()) false else { tones(v, e); true }
        }
    }

    private fun bind(v: TextView, e: EmojiData.Emoji?) {
        v.tag = e
        if (e == null) {
            v.text = ""
            v.background = null
            v.contentDescription = null
            v.isClickable = false
            return
        }
        val glyph = toned(a, e)
        val on = glyph in marked() || e.glyph in marked()
        v.text = glyph
        v.isClickable = true
        v.background = if (on) rounded(a.color(R.color.divider), a.dp(12).toFloat()) else null
        v.contentDescription = e.name + (if (on) ", chosen" else "") + (if (e.tones.isNotEmpty()) ", hold for skin tones" else "")
    }

    fun show() {
        val groups = EmojiData.groups(a)
        columns = ((a.resources.displayMetrics.widthPixels - a.dp(24)) / a.dp(48)).coerceIn(6, 12)
        val recents = recent(a).mapNotNull { r ->
            groups.firstNotNullOfOrNull { g -> g.emoji.find { it.glyph == r || r in it.tones } }
                ?.let { e -> EmojiData.Emoji(r, e.name, emptyList()) }
        }
        val sections = listOfNotNull(
            if (recents.isEmpty()) null else EmojiData.Group("Recently used", "🕘", recents),
        ) + groups
        browse(sections)

        val search = EditText(a).apply {
            hint = "Search emoji"
            textSize = 16f
            wrapping(newlines = false, max = 1)
            addTextChangedListener(object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                override fun afterTextChanged(s: Editable) {
                    val q = s.toString().trim()
                    if (q.isEmpty()) browse(sections) else find(q)
                }
            })
        }
        for ((i, g) in sections.withIndex()) {
            tabs.addView(TextView(a).apply {
                text = g.icon
                textSize = 20f
                gravity = Gravity.CENTER
                contentDescription = g.name
                setOnClickListener {
                    if (searching) search.text.clear()
                    list.setSelection(starts[i])
                    highlight(i)
                }
            }, LinearLayout.LayoutParams(0, a.dp(42), 1f))
        }
        list.apply {
            divider = null
            selector = android.graphics.drawable.ColorDrawable(android.graphics.Color.TRANSPARENT)
            this.adapter = this@EmojiPicker.adapter
            setOnScrollListener(object : AbsListView.OnScrollListener {
                override fun onScrollStateChanged(view: AbsListView?, state: Int) {}
                override fun onScroll(view: AbsListView?, first: Int, visible: Int, total: Int) {
                    if (!searching) highlight(starts.indexOfLast { it <= first }.coerceAtLeast(0))
                }
            })
        }
        val top = LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(a.label(title, 18f).apply { setTypeface(typeface, android.graphics.Typeface.BOLD) },
                LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(TextView(a).apply {
                text = if (stay) "Done" else "Close"
                textSize = 15f
                setTypeface(typeface, android.graphics.Typeface.BOLD)
                setTextColor(a.color(R.color.accent))
                setPadding(a.dp(12), a.dp(8), a.dp(4), a.dp(8))
                setOnClickListener { dialog.dismiss() }
            })
        }
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(12), 0, a.dp(12), 0)
            addView(top, matchWrap)
            addView(search, matchWrap)
            addView(tabs, matchWrap.apply { topMargin = a.dp(6) })
            addView(list, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, (a.resources.displayMetrics.heightPixels * 0.5).toInt()))
        }
        highlight(0)
        dialog = a.bottomSheet(box)
        dialog.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_HIDDEN or
            WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
        dialog.show()
    }

    /** Every section, one after another. */
    private fun browse(sections: List<EmojiData.Group>) {
        searching = false
        val out = mutableListOf<Row>()
        val at = mutableListOf<Int>()
        for (g in sections) {
            at += out.size
            out += Header(g.name)
            g.emoji.chunked(columns).forEach { out += Cells(it) }
        }
        rows = out
        starts = at
        adapter.notifyDataSetChanged()
    }

    private fun find(q: String) {
        searching = true
        val words = q.lowercase().split(' ').filter { it.isNotEmpty() }
        val hits = EmojiData.groups(a).flatMap { it.emoji }.filter { e -> words.all { it in e.name } }
        rows = listOf(Header(if (hits.isEmpty()) "No emoji named “$q”" else "Results")) + hits.chunked(columns).map { Cells(it) }
        adapter.notifyDataSetChanged()
        list.setSelection(0)
        highlight(-1)
    }

    private fun highlight(i: Int) {
        for (j in 0 until tabs.childCount) {
            tabs.getChildAt(j).background = if (j == i) rounded(a.color(R.color.divider), a.dp(12).toFloat()) else null
        }
    }

    private fun chose(glyph: String) {
        remember(a, glyph)
        pick(glyph)
        if (stay) adapter.notifyDataSetChanged() else dialog.dismiss()
    }
    /** The emoji and its skin tones in a row above it; the choice is remembered. */
    private fun tones(anchor: View, e: EmojiData.Emoji) {
        val row = LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            background = rounded(a.color(R.color.surface), a.dp(14).toFloat())
            elevation = a.dp(8).toFloat()
            setPadding(a.dp(4), a.dp(4), a.dp(4), a.dp(4))
        }
        val popup = PopupWindow(row, ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, true)
        for ((i, g) in (listOf(e.glyph) + e.tones).withIndex()) {
            row.addView(TextView(a).apply {
                text = g
                textSize = 28f
                gravity = Gravity.CENTER
                contentDescription = if (i == 0) e.name else "${e.name}, skin tone $i"
                setOnClickListener {
                    popup.dismiss()
                    // A single-tone variant becomes this emoji's default.
                    if (e.tones.size <= 5) setTone(a, e.glyph, i)
                    chose(g)
                }
            }, LinearLayout.LayoutParams(a.dp(48), a.dp(52)))
        }
        popup.elevation = a.dp(8).toFloat()
        popup.showAsDropDown(anchor, 0, -anchor.height - a.dp(64))
    }

    companion object {
        private const val PREFS = "emoji"
        private const val RECENT = 32

        fun recent(ctx: Context): List<String> =
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString("recent", "")!!
                .split('\n').filter { it.isNotEmpty() }

        fun remember(ctx: Context, glyph: String) {
            val list = (listOf(glyph) + recent(ctx)).distinct().take(RECENT)
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
                .putString("recent", list.joinToString("\n")).apply()
        }

        /** The emoji in the skin tone last chosen for it. */
        private fun toned(ctx: Context, e: EmojiData.Emoji): String {
            if (e.tones.isEmpty()) return e.glyph
            val i = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getInt("tone:${e.glyph}", 0)
            return if (i == 0) e.glyph else e.tones.getOrElse(i - 1) { e.glyph }
        }

        private fun setTone(ctx: Context, base: String, i: Int) =
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putInt("tone:$base", i).apply()
    }
}
