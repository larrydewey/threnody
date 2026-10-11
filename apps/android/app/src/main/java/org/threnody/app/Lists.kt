package org.threnody.app

import android.app.Activity
import android.content.Intent
import android.graphics.Typeface
import android.os.Bundle
import android.text.InputType
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.StrikethroughSpan
import android.text.style.StyleSpan
import android.text.style.TypefaceSpan
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.EditorInfo
import android.widget.CheckBox
import android.widget.EditText
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import uniffi.threnody_ffi.ListInfo
import uniffi.threnody_ffi.ListItemInfo
import uniffi.threnody_ffi.ThrenodyNode

/** Shared helpers for the lists screens. */
object Lists {
    /** Inline markdown: **bold**, *italic* or _italic_, ~~struck~~, `code`. */
    fun render(text: String): CharSequence {
        val out = SpannableStringBuilder()
        val re = Regex("""\*\*(.+?)\*\*|~~(.+?)~~|`(.+?)`|\*(.+?)\*|_(.+?)_""")
        var at = 0
        for (m in re.findAll(text)) {
            out.append(text, at, m.range.first)
            val start = out.length
            val (inner, span) = when {
                m.groups[1] != null -> m.groups[1]!!.value to StyleSpan(Typeface.BOLD)
                m.groups[2] != null -> m.groups[2]!!.value to StrikethroughSpan()
                m.groups[3] != null -> m.groups[3]!!.value to TypefaceSpan("monospace")
                m.groups[4] != null -> m.groups[4]!!.value to StyleSpan(Typeface.ITALIC)
                else -> m.groups[5]!!.value to StyleSpan(Typeface.ITALIC)
            }
            out.append(inner)
            out.setSpan(span, start, out.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            at = m.range.last + 1
        }
        out.append(text, at, text.length)
        return out
    }

    fun open(a: Activity, id: String) {
        a.startActivity(Intent(a, ListActivity::class.java).putExtra(ListActivity.ID, id))
    }

    /** Asks for text: a list's name, an item. */
    fun ask(a: Activity, title: String, initial: String, hint: String, done: (String) -> Unit) {
        val field = EditText(a).apply {
            setText(initial)
            this.hint = hint
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            setSelection(text.length)
        }
        SecureBuilder(a)
            .setTitle(title)
            .setView(LinearLayout(a).apply {
                setPadding(a.dp(20), a.dp(8), a.dp(20), 0)
                addView(field, MATCH_PARENT, WRAP_CONTENT)
            })
            .setPositiveButton("OK") { _, _ -> field.text.toString().trim().takeIf { it.isNotEmpty() }?.let(done) }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Picks a chat (contact or group) to share a list with. */
    fun share(a: Activity, node: ThrenodyNode, list: ListInfo, after: () -> Unit) {
        Threading.background {
            val chats = Threnody.conversations(node).filter { it.accepted && !it.blocked }.map { it.key to it.title } +
                node.groups().map { it.id to it.name }
            val choices = chats.filter { it.first !in list.sharedWith }
            a.runOnUiThread {
                if (choices.isEmpty()) {
                    Toast.makeText(a, "It's shared with every chat already.", Toast.LENGTH_SHORT).show()
                    return@runOnUiThread
                }
                SecureBuilder(a)
                    .setTitle("Share \"${list.title}\" with")
                    .setItems(choices.map { it.second }.toTypedArray()) { _, i ->
                        Threading.background {
                            try {
                                node.shareList(list.id, choices[i].first)
                                a.runOnUiThread {
                                    Toast.makeText(a, "Shared with ${choices[i].second}: everyone there can change it.",
                                        Toast.LENGTH_LONG).show()
                                }
                            } catch (e: Exception) {
                                a.runOnUiThread { Toast.makeText(a, "Couldn't share: ${e.message}", Toast.LENGTH_LONG).show() }
                            }
                            after()
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    fun copy(a: Activity, text: String) {
        a.getSystemService(android.content.ClipboardManager::class.java)
            ?.setPrimaryClip(android.content.ClipData.newPlainText("list", text))
        Toast.makeText(a, "Copied as markdown", Toast.LENGTH_SHORT).show()
    }
}

/** Every list: open one, or start a new one. */
class ListsActivity : Activity() {
    private lateinit var node: ThrenodyNode
    private lateinit var content: LinearLayout
    private var unsubscribe: (() -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val bar = TopBar(this) { finish() }.apply {
            title.text = "Lists"
            action(R.drawable.ic_add, "New list") { newList() }
        }
        content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(Design.screenPadding), dp(Design.md), dp(Design.screenPadding), dp(Design.xl))
        }
        val scroll = ScrollView(this).apply { addView(content, MATCH_PARENT, WRAP_CONTENT) }
        root.addView(bar, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, scroll)
        Threading.background {
            node = try { Threnody.start(this) } catch (_: Exception) { return@background runOnUiThread { finish() } }
            refresh()
        }
    }

    override fun onStart() {
        super.onStart()
        unsubscribe = Threnody.subscribe { e -> if (e is uniffi.threnody_ffi.NodeEvent.ListChanged) refresh() }
        if (::node.isInitialized) Threading.background { refresh() }
    }

    override fun onStop() {
        unsubscribe?.invoke()
        super.onStop()
    }

    private fun refresh() {
        if (!::node.isInitialized) return
        val lists = node.lists()
        val names = (Threnody.conversations(node).map { it.key to it.title } + node.groups().map { it.id to it.name }).toMap()
        runOnUiThread { show(lists, names) }
    }

    private fun show(lists: List<ListInfo>, names: Map<String, String>) {
        content.removeAllViews()
        if (lists.isEmpty()) {
            content.addView(emptyState(
                title = "No lists yet",
                message = "Shopping, gift ideas, packing… Lists stay on all your devices, and you can share one with a contact or a group so everyone can tick things off.",
                actionText = "New list",
                action = { newList() },
                iconRes = R.drawable.ic_list,
            ))
            return
        }
        for (l in lists) {
            val todo = l.items.filter { !it.heading }
            val done = todo.count { it.done }
            val shared = l.sharedWith.mapNotNull { names[it] }
            val detail = listOfNotNull(
                if (todo.isEmpty()) "Empty" else "$done of ${todo.size} done",
                if (shared.isNotEmpty()) "Shared with ${shared.joinToString(", ")}" else null,
            ).joinToString(" · ")
            content.addView(LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(Design.listItemPaddingH), dp(Design.listItemPaddingV), dp(Design.listItemPaddingH), dp(Design.listItemPaddingV))
                background = ripple()
                addView(label(l.title, Design.typeTitle, R.color.text, Design.weightMedium))
                addView(label(detail, Design.typeBodySmall, R.color.muted, Design.weightRegular))
                setOnClickListener { Lists.open(this@ListsActivity, l.id) }
                setOnLongClickListener { manage(l); true }
            }, matchWrap)
        }
    }

    private fun newList() {
        Lists.ask(this, "New list", "", "Groceries, Christmas ideas…") { title ->
            Threading.background {
                val id = try { node.createList(title) } catch (e: Exception) { return@background }
                runOnUiThread { Lists.open(this, id) }
            }
        }
    }

    private fun manage(l: ListInfo) {
        val options = listOf("Rename", "Share…", "Copy as markdown", "Delete")
        SecureBuilder(this)
            .setTitle(l.title)
            .setItems(options.toTypedArray()) { _, i ->
                when (options[i]) {
                    "Rename" -> Lists.ask(this, "Rename", l.title, "Name") { t ->
                        Threading.background { try { node.renameList(l.id, t) } catch (_: Exception) {}; refresh() }
                    }
                    "Share…" -> Lists.share(this, node, l) { refresh() }
                    "Copy as markdown" -> Threading.background {
                        val md = try { node.listMarkdown(l.id) } catch (_: Exception) { "" }
                        runOnUiThread { Lists.copy(this, md) }
                    }
                    else -> SecureBuilder(this)
                        .setTitle("Delete \"${l.title}\"?")
                        .setMessage("It goes from all your devices. Anyone you shared it with keeps their copy.")
                        .setPositiveButton("Delete") { _, _ -> Threading.background { node.deleteList(l.id); refresh() } }
                        .setNegativeButton("Cancel", null)
                        .show()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }
}

/** One list: tick, add, edit, reorder, nest and share its items. */
class ListActivity : Activity() {
    private lateinit var node: ThrenodyNode
    private lateinit var listId: String
    private lateinit var bar: TopBar
    private lateinit var content: LinearLayout
    private lateinit var add: EditText
    private var list: ListInfo? = null
    private var unsubscribe: (() -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        listId = intent.getStringExtra(ID) ?: return finish()
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        bar = TopBar(this) { finish() }.apply {
            title.text = "List"
            action(R.drawable.ic_more, "List options") { more() }
        }
        content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(Design.screenPadding), dp(Design.sm), dp(Design.screenPadding), dp(Design.xl))
        }
        val scroll = ScrollView(this).apply { addView(content, MATCH_PARENT, WRAP_CONTENT) }
        add = EditText(this).apply {
            hint = "Add an item"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            imeOptions = EditorInfo.IME_ACTION_DONE
            background = rounded(color(R.color.field), dp(22).toFloat())
            setPadding(dp(16), dp(10), dp(16), dp(10))
            setTextColor(color(R.color.text))
            setHintTextColor(color(R.color.muted))
            setOnEditorActionListener { _, action, ev ->
                if (action == EditorInfo.IME_ACTION_DONE || ev?.keyCode == KeyEvent.KEYCODE_ENTER) { addItem(); true } else false
            }
        }
        val addBar = LinearLayout(this).apply {
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(8), dp(6), dp(4), dp(6))
            addView(add, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(ImageButton(this@ListActivity).apply {
                setImageResource(R.drawable.ic_add)
                imageTintList = android.content.res.ColorStateList.valueOf(color(R.color.accent))
                background = ripple(borderless = true)
                contentDescription = "Add"
                setOnClickListener { addItem() }
            }, LinearLayout.LayoutParams(dp(48), dp(48)))
        }
        root.addView(bar, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        root.addView(addBar, matchWrap)
        setContentView(root)
        fitSystemBars(root, bar, addBar)
        Threading.background {
            node = try { Threnody.start(this) } catch (_: Exception) { return@background runOnUiThread { finish() } }
            refresh()
        }
    }

    override fun onStart() {
        super.onStart()
        unsubscribe = Threnody.subscribe { e ->
            if (e is uniffi.threnody_ffi.NodeEvent.ListChanged && e.id == listId) refresh()
        }
        if (::node.isInitialized) Threading.background { refresh() }
    }

    override fun onStop() {
        unsubscribe?.invoke()
        super.onStop()
    }

    private fun refresh() {
        if (!::node.isInitialized) return
        val l = node.list(listId)
        runOnUiThread {
            if (l == null) {
                Toast.makeText(this, "That list is gone.", Toast.LENGTH_SHORT).show()
                return@runOnUiThread finish()
            }
            list = l
            show(l)
        }
    }

    /** Runs a change on the worker thread, then redraws. */
    private fun change(f: (ThrenodyNode) -> Unit) {
        Threading.background {
            try { f(node) } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't change it: ${e.message}", Toast.LENGTH_SHORT).show() }
            }
            refresh()
        }
    }

    private fun show(l: ListInfo) {
        bar.title.text = l.title
        val todo = l.items.filter { !it.heading }
        bar.subtitle.text = if (todo.isEmpty()) "" else "${todo.count { it.done }} of ${todo.size} done" +
            (if (l.sharedWith.isNotEmpty()) " · shared" else "")
        bar.subtitle.visibility = if (bar.subtitle.text.isEmpty()) View.GONE else View.VISIBLE
        content.removeAllViews()
        if (l.items.isEmpty()) {
            content.addView(label("Nothing here yet. Add items below, or paste a markdown list from the menu.",
                Design.typeBodySmall, R.color.muted, Design.weightRegular).apply {
                setPadding(0, dp(24), 0, 0)
                gravity = Gravity.CENTER
            }, matchWrap)
        }
        rows.clear()
        for (item in l.items) {
            val r = row(item)
            rows += r
            content.addView(r, matchWrap)
            // Long-press and drag to move it; a long press alone opens its menu.
            DragReorder.attach(content, r, { rows }, { from, to -> moveItem(l, from, to) }) { itemMenu(item) }
        }
    }

    /** The item rows on screen, in order (for dragging). */
    private val rows = mutableListOf<View>()

    /** Moves the item at `from` so it ends up at `to`. */
    private fun moveItem(l: ListInfo, from: Int, to: Int) {
        val item = l.items.getOrNull(from) ?: return
        val others = l.items.filter { it.id != item.id }
        val after = if (to == 0) null else others.getOrNull(to - 1)?.id
        change { it.moveListItem(listId, item.id, after) }
    }

    private fun row(item: ListItemInfo): View {
        val text = TextView(this).apply {
            text = Lists.render(item.text)
            textSize = if (item.heading) 17f else 16f
            setTextColor(color(if (item.done) R.color.muted else R.color.text))
            if (item.heading) setTypeface(typeface, Typeface.BOLD)
            if (item.done) paintFlags = paintFlags or android.graphics.Paint.STRIKE_THRU_TEXT_FLAG
        }
        return LinearLayout(this).apply {
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8 + 24 * item.indent.toInt()), dp(if (item.heading) 14 else 2), dp(8), dp(2))
            minimumHeight = dp(48)
            background = ripple()
            if (!item.heading) addView(CheckBox(this@ListActivity).apply {
                isChecked = item.done
                contentDescription = item.text
                setOnCheckedChangeListener { _, on -> change { it.setListItemDone(listId, item.id, on) } }
            })
            addView(text, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = dp(4) })
            setOnClickListener {
                Lists.ask(this@ListActivity, if (item.heading) "Edit heading" else "Edit item", item.text, "Text") { t ->
                    change { it.editListItem(listId, item.id, t) }
                }
            }
        }
    }

    private fun itemMenu(item: ListItemInfo) {
        val items = list?.items ?: return
        val at = items.indexOfFirst { it.id == item.id }
        val options = buildList {
            if (at > 0) add("Move up")
            if (at < items.size - 1) add("Move down")
            if (!item.heading && item.indent < 3u) add("Indent")
            if (item.indent > 0u) add("Outdent")
            add("Add below")
            add("Delete")
        }
        SecureBuilder(this)
            .setTitle(item.text)
            .setItems(options.toTypedArray()) { _, i ->
                when (options[i]) {
                    // Up: after the one two above (or to the top).
                    "Move up" -> change { it.moveListItem(listId, item.id, if (at >= 2) items[at - 2].id else null) }
                    "Move down" -> change { it.moveListItem(listId, item.id, items[at + 1].id) }
                    "Indent" -> change { it.indentListItem(listId, item.id, (item.indent + 1u).toUByte()) }
                    "Outdent" -> change { it.indentListItem(listId, item.id, (item.indent - 1u).toUByte()) }
                    "Add below" -> Lists.ask(this, "Add below", "", "Item") { t ->
                        change { it.addListItem(listId, item.id, t, false) }
                    }
                    else -> change { it.deleteListItem(listId, item.id) }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun addItem() {
        val t = add.text.toString().trim()
        if (t.isEmpty()) return
        add.setText("")
        val last = list?.items?.lastOrNull()?.id
        // "# Heading" makes a section.
        val heading = t.startsWith("#")
        change { it.addListItem(listId, last, t.trimStart('#').trim(), heading) }
    }

    private fun more() {
        val l = list ?: return
        val options = listOf("Rename", "Share…", "Add a heading", "Paste a markdown list", "Copy as markdown", "Clear ticked items", "Delete list")
        SecureBuilder(this)
            .setTitle(l.title)
            .setItems(options.toTypedArray()) { _, i ->
                when (options[i]) {
                    "Rename" -> Lists.ask(this, "Rename", l.title, "Name") { t -> change { it.renameList(listId, t) } }
                    "Share…" -> Lists.share(this, node, l) { refresh() }
                    "Add a heading" -> Lists.ask(this, "Add a heading", "", "Produce, Dairy…") { t ->
                        change { it.addListItem(listId, l.items.lastOrNull()?.id, t, true) }
                    }
                    "Paste a markdown list" -> {
                        val clip = getSystemService(android.content.ClipboardManager::class.java)
                            ?.primaryClip?.getItemAt(0)?.coerceToText(this)?.toString().orEmpty()
                        val field = EditText(this).apply {
                            setText(clip)
                            hint = "- [ ] milk\n- [x] eggs\n## Produce\n- apples"
                            minLines = 5
                            gravity = Gravity.TOP
                        }
                        SecureBuilder(this)
                            .setTitle("Paste a markdown list")
                            .setView(LinearLayout(this).apply {
                                setPadding(dp(20), dp(8), dp(20), 0)
                                addView(field, MATCH_PARENT, WRAP_CONTENT)
                            })
                            .setPositiveButton("Add") { _, _ ->
                                val md = field.text.toString()
                                change { it.addListMarkdown(listId, md) }
                            }
                            .setNegativeButton("Cancel", null)
                            .show()
                    }
                    "Copy as markdown" -> Threading.background {
                        val md = try { node.listMarkdown(listId) } catch (_: Exception) { "" }
                        runOnUiThread { Lists.copy(this, md) }
                    }
                    "Clear ticked items" -> change { it.clearDoneItems(listId) }
                    else -> SecureBuilder(this)
                        .setTitle("Delete \"${l.title}\"?")
                        .setMessage("It goes from all your devices. Anyone you shared it with keeps their copy.")
                        .setPositiveButton("Delete") { _, _ -> Threading.background { node.deleteList(listId); runOnUiThread { finish() } } }
                        .setNegativeButton("Cancel", null)
                        .show()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    companion object {
        const val ID = "list"
    }
}
