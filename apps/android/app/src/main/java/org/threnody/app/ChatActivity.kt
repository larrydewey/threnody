package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Typeface
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.text.InputType
import android.text.format.DateFormat
import android.text.format.Formatter
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.EditText
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.util.Date
import java.util.concurrent.Executors
import uniffi.threnody_ffi.FileOptions
import uniffi.threnody_ffi.GroupInfo
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.NodeEvent
import uniffi.threnody_ffi.ThrenodyNode

/**
 * One conversation, with a contact (extras [KEY] and [DEVICE]) or a group
 * ([GROUP]): its messages, a compose bar, and the conversation's settings.
 */
class ChatActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var node: ThrenodyNode
    /** The group id, for a group conversation. */
    private var group: String? = null
    private var info: GroupInfo? = null
    private var key = ""
    /** The device to address; any of the account's devices reaches them all. */
    private var device = ""
    private var convo: Conversation? = null
    private lateinit var bar: TopBar
    private lateinit var banner: LinearLayout
    private lateinit var messages: LinearLayout
    private lateinit var scroll: ScrollView
    private lateinit var compose: EditText
    private lateinit var composeBar: LinearLayout
    /** Messages being sent, shown until history has them. */
    private val pending = mutableListOf<String>()
    private var unsubscribe: (() -> Unit)? = null
    private var myDevice = ""

    /** A history entry, or a message still being sent. */
    private data class Item(val entry: HistoryEntry, val sending: Boolean = false)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        group = intent.getStringExtra(GROUP)
        key = group ?: intent.getStringExtra(KEY) ?: return finish()
        device = intent.getStringExtra(DEVICE) ?: key

        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        bar = TopBar(this) { finish() }.apply {
            title.text = if (group != null) "Group" else Threnody.short(device)
            action(R.drawable.ic_more, if (group != null) "Group options" else "Contact options") { more(it) }
        }
        banner = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(color(R.color.surface))
            setPadding(dp(16), dp(12), dp(16), dp(12))
            visibility = View.GONE
        }
        messages = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(12), dp(8), dp(12), dp(8))
        }
        scroll = ScrollView(this).apply {
            isFillViewport = true
            addView(messages, MATCH_PARENT, WRAP_CONTENT)
        }
        composeBar = composeBar()
        root.addView(bar, matchWrap)
        root.addView(banner, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        root.addView(composeBar, matchWrap)
        setContentView(root)
        fitSystemBars(root, bar, composeBar)
        // Keep the newest message in view when the keyboard opens.
        scroll.addOnLayoutChangeListener { _, _, _, _, b, _, _, _, ob -> if (b < ob) toBottom() }

        worker.execute {
            node = Threnody.start(this)
            refresh()
        }
    }

    private fun composeBar(): LinearLayout {
        compose = EditText(this).apply {
            hint = "Message"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
                InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            maxLines = 5
            background = rounded(color(R.color.surface), dp(22).toFloat())
            setPadding(dp(16), dp(10), dp(16), dp(10))
            setTextColor(color(R.color.text))
            setHintTextColor(color(R.color.muted))
        }
        fun button(res: Int, label: String, tint: Int, onClick: (View) -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(color(tint))
            contentDescription = label
            tooltipText = label
            background = getDrawable(android.R.drawable.list_selector_background)
            setOnClickListener { onClick(it) }
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.BOTTOM
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(4), dp(6), dp(4), dp(6))
            addView(button(R.drawable.ic_attach, "Send photos or files", R.color.muted) { attach(it) }, LinearLayout.LayoutParams(dp(48), dp(48)))
            addView(compose, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { bottomMargin = dp(2) })
            addView(button(R.drawable.ic_send, "Send", R.color.accent) { send() }, LinearLayout.LayoutParams(dp(48), dp(48)))
        }
    }

    override fun onStart() {
        super.onStart()
        Threnody.visible++
        Threnody.visibleChat = setOf(key, device) + (convo?.devices ?: emptyList())
        ThrenodyService.clearNotification(this, key)
        unsubscribe = Threnody.subscribe { e -> if (concerns(e)) worker.execute { refresh() } }
        if (::node.isInitialized) worker.execute { refresh() }
    }

    override fun onStop() {
        Threnody.visible--
        Threnody.visibleChat = emptySet()
        unsubscribe?.invoke()
        super.onStop()
    }

    /** Whether an event is about this conversation (cheap check, then by account). */
    private fun concerns(e: NodeEvent): Boolean {
        if (e is NodeEvent.HistorySynced) return true
        group?.let { g ->
            return when (e) {
                is NodeEvent.GroupMessage -> e.group == g
                is NodeEvent.GroupFile -> e.group == g
                is NodeEvent.GroupMembersChanged -> e.group == g
                is NodeEvent.GroupJoined -> e.group == g
                is NodeEvent.GroupLeft -> e.group == g
                is NodeEvent.Delivered -> e.group == g
                else -> false
            }
        }
        val peer = when (e) {
            is NodeEvent.Message -> e.peer
            is NodeEvent.MessageRequest -> e.peer
            is NodeEvent.MessagesDeleted -> e.peer
            is NodeEvent.MessageEdited -> e.peer
            is NodeEvent.File -> e.peer
            is NodeEvent.Connected -> e.peer
            is NodeEvent.Disconnected -> e.peer
            is NodeEvent.ApprovalChanged -> e.peer
            is NodeEvent.Delivered -> e.peer
            is NodeEvent.AccountChanged -> return true
            else -> return false
        }
        // A device we haven't seen yet may belong to this account.
        return peer == device || convo?.devices?.contains(peer) == true ||
            (::node.isInitialized && Threnody.key(node.contacts(), peer) == key)
    }

    /** Reloads the conversation's state and messages (on the worker thread). */
    private fun refresh() {
        val g = group
        val history: List<HistoryEntry>
        var c: Conversation? = null
        var gi: GroupInfo? = null
        if (g != null) {
            gi = node.groups().firstOrNull { it.id == g }
            history = try { node.groupHistory(g, 200u) } catch (_: Exception) { emptyList() }
        } else {
            // The key moves from device to account once the account is known.
            c = Threnody.conversations(node).firstOrNull { it.key == key || device in it.devices }
            if (c != null) {
                key = c.key
                device = c.device
            }
            history = try { node.history(device, 200u) } catch (_: Exception) { emptyList() }
        }
        val me = node.deviceFingerprint()
        myDevice = me
        val items = history.map { Item(it) } + synchronized(pending) {
            pending.map { Item(HistoryEntry(atMs = ULong.MAX_VALUE, outgoing = true, device = me, text = it, disappearing = false, file = null, delivered = false, id = 0uL, edited = false, recipients = 0u, deliveredTo = 0u), sending = true) }
        }
        val names = if (g != null) history.map { it.device }.distinct().associateWith { Threnody.nameOf(node, it) } else emptyMap()
        runOnUiThread {
            convo = c
            info = gi
            if (Threnody.visibleChat.isNotEmpty()) {
                Threnody.visibleChat = setOf(key, device) + (c?.devices ?: emptyList())
            }
            if (g != null) groupHeader(gi) else header(c)
            show(items, names)
        }
    }

    private fun header(c: Conversation?) {
        bar.title.text = c?.title ?: Threnody.short(device)
        bar.subtitle.visibility = View.VISIBLE
        bar.subtitle.text = listOfNotNull(
            if (c?.connected == true) "connected" else "not connected",
            if (c?.verified == true) "verified" else "⚠ not verified",
            c?.devices?.size?.takeIf { it > 1 }?.let { "$it devices" },
        ).joinToString(" · ")
        bar.subtitle.setTextColor(color(if (c?.verified == true) R.color.muted else R.color.warning))
        clearBanner()
        if (c == null) return
        composeBar.visibility = if (c.accepted) View.VISIBLE else View.GONE
        if (!c.accepted) {
            banner(
                "${c.title} wants to message you. They can't tell whether you've read this. " +
                    "Accept to reply; block to stop them for good.",
                "Accept" to { request("accept") },
                "Block" to { request("block") },
                "Delete" to { request("delete") },
                warning = true,
            )
            return
        }
        when {
            // Someone we'd verified now has a device we haven't: a key we
            // don't know is in the conversation.
            !c.verified && c.anyVerified -> banner(
                "⚠ ${c.title} added a device you haven't verified. Until you compare its safety number, " +
                    "someone else could be reading as them.",
                "Compare" to { safety() },
                warning = true,
            )
            !c.approved -> banner(
                "Approve ${c.title} once you trust them, and compare safety numbers to make sure no one is in " +
                    "the middle. Mutually approved contacts find each other nearby, relay for each other and " +
                    "hold each other's messages while offline.",
                "Approve" to { setApproval(true) },
                "Compare" to { safety() },
            )
            !c.verified -> banner(
                "⚠ Not verified. Compare safety numbers with ${c.title} to make sure no one is in the middle.",
                "Compare" to { safety() },
                warning = true,
            )
        }
    }

    private fun groupHeader(g: GroupInfo?) {
        clearBanner()
        bar.subtitle.visibility = View.VISIBLE
        if (g == null) {
            val invite = try { node.groupInvites().firstOrNull { it.group == group } } catch (_: Exception) { null }
            if (invite != null) {
                bar.title.text = invite.name
                bar.subtitle.text = "invitation"
                banner(
                    "${Threnody.nameOf(node, invite.from)} invites you to this group. Members see each other's " +
                        "messages and who else is in it.",
                    "Join" to { joinGroup() },
                    "Decline" to { declineGroup() },
                )
            } else {
                bar.subtitle.text = "not a member"
                banner("You're no longer in this group. The owner can invite you again.")
            }
            composeBar.visibility = View.GONE
            return
        }
        composeBar.visibility = View.VISIBLE
        bar.title.text = g.name
        bar.subtitle.text = listOfNotNull(
            members(g.members.size),
            if (g.owned) "you're the owner" else null,
        ).joinToString(" · ")
        if (g.owned && g.members.size == 1) {
            banner("Only you are in this group. Invite contacts to it.", "Invite" to { inviteToGroup() })
        }
    }

    private fun clearBanner() {
        banner.removeAllViews()
        banner.visibility = View.GONE
    }

    private fun banner(text: String, vararg actions: Pair<String, () -> Unit>, warning: Boolean = false) {
        banner.visibility = View.VISIBLE
        banner.addView(label(text, 14f, if (warning) R.color.warning else R.color.muted), matchWrap)
        if (actions.isEmpty()) return
        val row = LinearLayout(this)
        for ((name, onClick) in actions) {
            row.addView(label(name, 15f, R.color.accent).apply {
                setTypeface(typeface, Typeface.BOLD)
                setPadding(0, dp(10), dp(24), dp(4))
                minHeight = dp(40)
                gravity = Gravity.CENTER_VERTICAL
                setOnClickListener { onClick() }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        banner.addView(row, matchWrap)
    }

    private fun show(items: List<Item>, names: Map<String, String>) {
        val atEnd = !scroll.canScrollVertically(1)
        messages.removeAllViews()
        if (items.isEmpty()) {
            val text = if (group != null) "No messages yet. Group messages are end-to-end encrypted with MLS."
            else "No messages yet. Messages are end-to-end encrypted and stored encrypted on this device."
            messages.addView(label(text, 14f, R.color.muted).apply { gravity = Gravity.CENTER; setPadding(dp(24), dp(48), dp(24), 0) }, matchWrap)
        }
        var lastSender: String? = null
        for (run in albums(items)) {
            val e = run.first().entry
            // In groups, name the sender above the first of their run of messages.
            val sender = if (group != null && !e.outgoing && e.device != lastSender) names[e.device] else null
            lastSender = if (e.outgoing) null else e.device
            messages.addView(bubble(run, sender))
        }
        if (atEnd || items.lastOrNull()?.sending == true) toBottom()
    }

    // Not fullScroll(): that moves focus to the last bubble, away from the compose field.
    private fun toBottom() = scroll.post { scroll.scrollTo(0, messages.height) }

    /** Consecutive files of one album from one sender go in one bubble. */
    private fun albums(items: List<Item>): List<List<Item>> {
        val out = mutableListOf<MutableList<Item>>()
        for (item in items) {
            val album = item.entry.file?.album ?: 0uL
            val last = out.lastOrNull()?.last()?.entry
            if (album != 0uL && last?.file?.album == album && last.device == item.entry.device) {
                out.last().add(item)
            } else {
                out.add(mutableListOf(item))
            }
        }
        return out
    }

    /** Photos of one message: one large, or a grid of several. */
    private fun photos(run: List<Item>): View {
        val files = run.mapNotNull { it.entry.file?.let { f -> f to it.entry } }
        val width = minOf(dp(260), resources.displayMetrics.widthPixels - dp(120))
        val single = files.size == 1
        val side = if (single) width else (width - dp(4)) / 2
        val grid = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        for (row in files.chunked(if (single) 1 else 2)) {
            val line = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
            for ((i, pair) in row.withIndex()) {
                val (f, e) = pair
                val cell = photo(f, e, side, if (single) dp(320) else side)
                line.addView(cell, LinearLayout.LayoutParams(side, if (single) WRAP_CONTENT else side).apply {
                    if (i == 1) marginStart = dp(4)
                })
            }
            grid.addView(line, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { bottomMargin = dp(4) })
        }
        return grid
    }

    /**
     * One photo. A sensitive one is an opaque cover: it isn't even decoded
     * until opened, so nothing of it shows in the chat.
     */
    private fun photo(f: uniffi.threnody_ffi.FileInfo, e: HistoryEntry, width: Int, limit: Int): View {
        val location = f.location
        val open = View.OnClickListener {
            if (location == null) return@OnClickListener
            startActivity(Intent(this, ImageActivity::class.java)
                .putExtra(ImageActivity.LOCATION, location)
                .putExtra(ImageActivity.NAME, f.name)
                .putExtra(ImageActivity.CAPTION, e.text))
        }
        if (f.sensitive || location == null) {
            return label(if (location == null) "📷\nnot available" else "🔒\nSensitive photo\nTap to view", 14f, R.color.bubble_in).apply {
                gravity = Gravity.CENTER
                setTypeface(typeface, Typeface.BOLD)
                // Opaque, in the text colour: unmistakably covered, in either theme.
                background = rounded(color(R.color.text), dp(12).toFloat())
                height = if (limit == width) width else dp(180)
                contentDescription = if (location == null) "Photo not available" else "Sensitive photo, tap to view"
                setOnClickListener(open)
            }
        }
        val view = ImageView(this).apply {
            adjustViewBounds = true
            maxHeight = limit
            scaleType = if (limit == width) ImageView.ScaleType.CENTER_CROP else ImageView.ScaleType.FIT_CENTER
            background = rounded(color(R.color.surface), dp(12).toFloat())
            clipToOutline = true
            minimumHeight = dp(120)
            contentDescription = e.text.ifBlank { "Photo" }
            setOnClickListener(open)
        }
        Media.thumbnail(this, location, width) { b -> runOnUiThread { view.setImageBitmap(b); view.minimumHeight = 0 } }
        return view
    }

    private fun bubble(run: List<Item>, sender: String?): View {
        val item = run.first()
        val e = item.entry
        val outgoing = e.outgoing
        val fg = color(if (outgoing) R.color.on_bubble_out else R.color.text)
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = rounded(color(if (outgoing) R.color.bubble_out else R.color.bubble_in), dp(18).toFloat())
            setPadding(dp(14), dp(8), dp(14), dp(6))
        }
        if (sender != null) {
            body.addView(label(sender, 13f, R.color.accent).apply { setTypeface(typeface, Typeface.BOLD) })
        }
        val file = e.file
        val time = if (item.sending) "sending…" else time(e.atMs.toLong())
        val pictures = run.all { it.entry.file?.let { f -> Media.isImage(f.name) } == true }
        val meta = if (file != null && pictures) {
            body.setPadding(dp(4), dp(4), dp(4), dp(6))
            body.addView(photos(run))
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    text = e.text
                    textSize = 16f
                    setTextColor(fg)
                    setTextIsSelectable(true)
                    setPadding(dp(10), dp(4), dp(10), 0)
                })
            }
            time
        } else if (file == null) {
            body.addView(TextView(this).apply {
                text = e.text
                textSize = 16f
                setTextColor(fg)
                setTextIsSelectable(true)
            })
            time
        } else {
            for (r in run) {
                val f = r.entry.file ?: continue
                body.addView(TextView(this).apply {
                    text = (if (f.sensitive) "📎 Sensitive file: " else "📎 ") + f.name
                    textSize = 16f
                    setTextColor(fg)
                    setTypeface(typeface, Typeface.BOLD)
                    val uri = f.location?.let(Uri::parse)
                    if (uri != null) setOnClickListener { open(uri) }
                })
            }
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    text = e.text
                    textSize = 16f
                    setTextColor(fg)
                    setTextIsSelectable(true)
                })
            }
            Formatter.formatShortFileSize(this, run.sumOf { it.entry.file?.size ?: 0uL }.toLong()) + " · " + time +
                if (file.location != null && !outgoing) " · in Downloads" else ""
        }
        // One tick once sent, two once delivered: to a device of the
        // contact, or to every member of a group (with a count until then).
        // Sent from another of our devices: that device sees its ticks.
        val elsewhere = outgoing && !item.sending && e.device != myDevice
        val tick = when {
            !outgoing || item.sending -> ""
            elsewhere -> " · from your other device"
            run.all { it.entry.delivered } -> " ✓✓"
            group != null && e.deliveredTo > 0u -> " ✓ ${e.deliveredTo}/${e.recipients}"
            else -> " ✓"
        }
        body.addView(TextView(this).apply {
            text = meta + (if (e.edited) " · edited" else "") + (if (e.disappearing) " · ⏱" else "") + tick
            contentDescription = text.toString().replace("✓✓", "delivered").replace(" ✓ ", " delivered to ").replace("✓", "sent")
            textSize = 11f
            setTextColor(fg)
            alpha = 0.7f
            gravity = Gravity.END
        }, matchWrap)
        if (!item.sending) {
            val longPress = View.OnLongClickListener { deleteMessage(run.map { it.entry }); true }
            body.setOnLongClickListener(longPress)
            // Photos take the long press too, not just the bubble's edge.
            fun all(v: View) {
                if (v is ImageView || (v is TextView && v.hasOnClickListeners())) v.setOnLongClickListener(longPress)
                if (v is android.view.ViewGroup) for (i in 0 until v.childCount) all(v.getChildAt(i))
            }
            all(body)
        }
        return LinearLayout(this).apply {
            gravity = if (outgoing) Gravity.END else Gravity.START
            setPadding(if (outgoing) dp(48) else 0, dp(3), if (outgoing) 0 else dp(48), dp(3))
            addView(body, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
    }

    /** Delete for me, or (our own 1:1 messages) for everyone; an album goes whole. */
    private fun deleteMessage(entries: List<HistoryEntry>) {
        val e = entries.first()
        val g = group
        val mine = g == null && e.outgoing && e.device == myDevice && entries.all { it.id != 0uL }
        val canEdit = mine && e.file == null
        val options = buildList {
            if (canEdit) add("Edit")
            add("Delete for me")
            if (mine) add("Delete for everyone")
        }
        AlertDialog.Builder(this)
            .setItems(options.toTypedArray()) { _, i ->
                if (options[i] == "Edit") return@setItems editMessage(e)
                val everyone = options[i] == "Delete for everyone"
                worker.execute {
                    run("delete") {
                        when {
                            g != null -> entries.forEach { node.deleteGroupEntry(g, it.atMs, it.device) }
                            entries.all { it.id != 0uL } -> node.deleteMessages(device, entries.map { it.id }, everyone)
                            else -> entries.forEach { node.deleteEntry(device, it.atMs, it.device) }
                        }
                    }
                    entries.forEach { Media.forget(this, it.file?.location) }
                    if (everyone) runOnUiThread {
                        Toast.makeText(this, "Deleted. Their devices delete it too, if they're running a current version.",
                            Toast.LENGTH_LONG).show()
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun editMessage(e: HistoryEntry) {
        val field = EditText(this).apply {
            setText(e.text)
            setSelection(e.text.length)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
                InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
        }
        AlertDialog.Builder(this)
            .setTitle("Edit message")
            .setMessage("They see it marked as edited, if they're running a current version.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val body = field.text.toString().trim()
                if (body.isEmpty() || body == e.text) return@setPositiveButton
                worker.execute { run("edit") { node.editMessage(device, e.id, body) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun time(ms: Long) = DateFormat.getTimeFormat(this).format(Date(ms))

    private fun send() {
        val text = compose.text.toString().trim()
        if (text.isEmpty() || !::node.isInitialized) return
        compose.setText("")
        synchronized(pending) { pending.add(text) }
        worker.execute { refresh() }
        worker.execute {
            val g = group
            val error = try {
                if (g != null) {
                    node.sendGroupText(g, text)
                    null
                } else if (node.sendText(device, text) == 0u) {
                    // Reaches the contact directly, by relay, or sealed for mailboxes.
                    "Not delivered: they're offline and no mutual contact can hold it."
                } else null
            } catch (e: Exception) {
                "Couldn't send: ${e.message}"
            }
            synchronized(pending) { pending.remove(text) }
            refresh()
            if (error != null) runOnUiThread {
                Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                if (compose.text.isEmpty()) compose.setText(text)
            }
        }
    }

    private fun attach(anchor: View) {
        PopupMenu(this, anchor).apply {
            menu.add("Photos").setOnMenuItemClickListener { pick(photos = true); true }
            menu.add("File").setOnMenuItemClickListener { pick(photos = false); true }
        }.show()
    }

    private fun pick(photos: Boolean) {
        val intent = if (photos && android.os.Build.VERSION.SDK_INT >= 33) {
            // The system photo picker: no storage permission needed.
            Intent(android.provider.MediaStore.ACTION_PICK_IMAGES)
                .putExtra(android.provider.MediaStore.EXTRA_PICK_IMAGES_MAX, MAX_PICK)
        } else {
            Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                .setType(if (photos) "image/*" else "*/*")
                .putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
        startActivityForResult(intent, PICK_FILE)
    }

    /** A picked file, read and ready to send. */
    private class Picked(val name: String, val data: ByteArray, val uri: Uri)

    @Deprecated("Activity result API needs AndroidX; this app uses the platform only.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_FILE || resultCode != RESULT_OK || data == null) return
        val clip = data.clipData
        val uris = if (clip != null) (0 until clip.itemCount).map { clip.getItemAt(it).uri } else listOfNotNull(data.data)
        if (uris.isEmpty()) return
        worker.execute {
            try {
                val max = node.maxFileSize().toLong()
                val picked = uris.take(MAX_PICK).map { uri ->
                    // Keep access so a sent file can be opened from the chat later.
                    try { contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) } catch (_: Exception) {}
                    val (name, size) = contentResolver.query(uri, null, null, null, null)?.use { c ->
                        c.moveToFirst()
                        c.getString(c.getColumnIndexOrThrow(OpenableColumns.DISPLAY_NAME)) to
                            c.getLong(c.getColumnIndexOrThrow(OpenableColumns.SIZE))
                    } ?: ("file" to -1L)
                    if (size > max * 2) throw IllegalArgumentException("$name is too large")
                    val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                        ?: throw IllegalArgumentException("couldn't read $name")
                    val (n, d) = Media.prepare(this, name, bytes)
                    if (d.size > max) {
                        throw IllegalArgumentException("$n is larger than " + Formatter.formatShortFileSize(this, max))
                    }
                    Picked(n, d, uri)
                }
                runOnUiThread { sendSheet(picked) }
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't send: ${e.message}", Toast.LENGTH_LONG).show() }
            }
        }
    }

    /** Previews what's about to go, with a caption and the sensitive choice. */
    private fun sendSheet(picked: List<Picked>) {
        val images = picked.count { Media.isImage(it.name) }
        val strip = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for (p in picked) {
            val cell = if (Media.isImage(p.name)) {
                ImageView(this).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                    clipToOutline = true
                    contentDescription = p.name
                    worker.execute {
                        val b = Media.decode(android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(p.data)), dp(160))
                        runOnUiThread { setImageBitmap(b) }
                    }
                }
            } else {
                label("📎\n${p.name}", 12f, R.color.text).apply {
                    gravity = Gravity.CENTER
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                }
            }
            strip.addView(cell, LinearLayout.LayoutParams(dp(80), dp(80)).apply { marginEnd = dp(6) })
        }
        val caption = EditText(this).apply {
            hint = "Add a message"
            setText(compose.text)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
                InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            maxLines = 4
        }
        val sensitive = android.widget.CheckBox(this).apply {
            text = if (images > 0) "Sensitive: they see it covered until they tap it" else "Sensitive: shown covered until opened"
        }
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), 0)
            addView(android.widget.HorizontalScrollView(this@ChatActivity).apply { addView(strip) }, matchWrap)
            addView(caption, matchWrap)
            addView(sensitive, matchWrap)
            if (images > 0 && Privacy.stripMetadata(this@ChatActivity)) {
                addView(label("Location and camera details are removed before sending.", 12f, R.color.muted), matchWrap)
            }
        }
        val what = when {
            images == picked.size -> if (images == 1) "1 photo" else "$images photos"
            picked.size == 1 -> "1 file"
            else -> "${picked.size} files"
        }
        AlertDialog.Builder(this)
            .setTitle("Send $what")
            .setView(android.widget.ScrollView(this).apply { addView(body) })
            .setPositiveButton("Send") { _, _ ->
                compose.setText("")
                sendFiles(picked, caption.text.toString().trim(), sensitive.isChecked)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun sendFiles(picked: List<Picked>, caption: String, sensitive: Boolean) {
        val album = if (picked.size > 1) uniffi.threnody_ffi.albumId() else 0uL
        worker.execute {
            val g = group
            var failed: String? = null
            for ((i, p) in picked.withIndex()) {
                val options = FileOptions(sensitive, if (i == 0) caption else "", album)
                // Keep our own copy of a photo privately, so the chat can show it.
                val location = if (Media.isImage(p.name)) Media.savePrivate(this, p.name, p.data) else p.uri.toString()
                try {
                    if (g != null) node.sendGroupFile(g, p.name, p.data, location, options)
                    else node.sendFile(device, p.name, p.data, location, options)
                } catch (e: Exception) {
                    Media.forget(this, location)
                    failed = e.message
                }
                refresh()
            }
            if (failed != null) runOnUiThread {
                Toast.makeText(this, "Couldn't send: $failed", Toast.LENGTH_LONG).show()
            }
        }
    }

    private fun open(uri: Uri) {
        try {
            startActivity(Intent(Intent.ACTION_VIEW, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        } catch (_: Exception) {
            Toast.makeText(this, "No app can open this file", Toast.LENGTH_SHORT).show()
        }
    }

    private fun more(anchor: View) {
        if (group != null) return groupMenu(anchor)
        val c = convo
        PopupMenu(this, anchor).apply {
            menu.add("Rename").setOnMenuItemClickListener { rename(); true }
            menu.add("Safety number").setOnMenuItemClickListener { safety(); true }
            if (c?.approved == true) {
                menu.add("Revoke approval").setOnMenuItemClickListener { setApproval(false); true }
            } else {
                menu.add("Approve").setOnMenuItemClickListener { setApproval(true); true }
            }
            menu.add("Disappearing messages").setOnMenuItemClickListener { disappearing(); true }
            if (c?.connected == true && c.approved) {
                menu.add("Faster link (Wi-Fi Direct)").setOnMenuItemClickListener { wifiDirect(); true }
            }
            show()
        }
    }

    private fun groupMenu(anchor: View) {
        val g = info ?: return
        PopupMenu(this, anchor).apply {
            menu.add("Members").setOnMenuItemClickListener { members(); true }
            if (g.owned) menu.add("Invite contacts").setOnMenuItemClickListener { inviteToGroup(); true }
            menu.add("Disappearing messages").setOnMenuItemClickListener { disappearing(); true }
            menu.add(if (g.owned) "Delete group" else "Leave group").setOnMenuItemClickListener { leaveGroup(); true }
            show()
        }
    }

    /** The members, by name; the owner can remove them. */
    private fun members() {
        val g = info ?: return
        worker.execute {
            // One row per account: a contact's devices are invited and removed together.
            val contacts = node.contacts()
            val me = node.deviceFingerprint()
        myDevice = me
            val rows = g.members.groupBy { fp -> if (fp == me) me else Threnody.key(contacts, fp) }
                .map { (_, devices) -> devices.first() }
            val labels = rows.map { fp ->
                Threnody.nameOf(node, fp) + if (fp == g.owner) " (owner)" else ""
            }
            runOnUiThread {
                val d = AlertDialog.Builder(this)
                    .setTitle(members(g.members.size))
                    .setNegativeButton("Close", null)
                if (g.owned) {
                    d.setItems(labels.toTypedArray()) { _, i ->
                        if (rows[i] != me) removeMember(rows[i], labels[i])
                    }
                } else {
                    d.setItems(labels.toTypedArray(), null)
                }
                d.show()
            }
        }
    }

    private fun removeMember(fp: String, name: String) {
        val g = group ?: return
        AlertDialog.Builder(this)
            .setTitle("Remove $name?")
            .setMessage("They stop receiving new messages in ${info?.name}. You can invite them again later.")
            .setPositiveButton("Remove") { _, _ -> worker.execute { run("remove") { node.removeFromGroup(g, fp) } } }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun inviteToGroup() {
        val g = info ?: return
        worker.execute {
            val candidates = Threnody.conversations(node).filter { c -> c.devices.none { it in g.members } }
            runOnUiThread {
                if (candidates.isEmpty()) {
                    Toast.makeText(this, "All your contacts are already in this group", Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                val chosen = BooleanArray(candidates.size)
                AlertDialog.Builder(this)
                    .setTitle("Invite to ${g.name}")
                    .setMultiChoiceItems(candidates.map { it.title }.toTypedArray(), chosen) { _, i, on -> chosen[i] = on }
                    .setPositiveButton("Invite") { _, _ ->
                        val picked = candidates.filterIndexed { i, _ -> chosen[i] }
                        worker.execute {
                            for (c in picked) run("invite ${c.title}") { node.inviteToGroup(g.id, c.device) }
                            if (picked.any { !it.approved }) runOnUiThread {
                                Toast.makeText(this, "Contacts who haven't approved you are asked before they join.",
                                    Toast.LENGTH_LONG).show()
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun leaveGroup() {
        val g = info ?: return
        AlertDialog.Builder(this)
            .setTitle(if (g.owned) "Delete ${g.name}?" else "Leave ${g.name}?")
            .setMessage(
                if (g.owned) "Everyone is removed and the group ends. Its history stays on your devices."
                else "You stop receiving its messages. Its history stays on your devices; the owner can invite you again.",
            )
            .setPositiveButton(if (g.owned) "Delete" else "Leave") { _, _ ->
                worker.execute {
                    run(if (g.owned) "delete the group" else "leave") { node.leaveGroup(g.id) }
                    runOnUiThread { finish() }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Answers a message request: accept, block or delete. */
    private fun request(what: String) {
        worker.execute {
            run(what) {
                when (what) {
                    "accept" -> node.acceptContact(device)
                    "block" -> node.blockContact(device)
                    else -> node.deleteRequest(device)
                }
            }
            if (what != "accept") runOnUiThread { finish() }
        }
    }

    private fun declineGroup() {
        val g = group ?: return
        worker.execute {
            run("decline") { node.declineGroupInvite(g) }
            runOnUiThread { finish() }
        }
    }

    private fun joinGroup() {
        val g = group ?: return
        worker.execute { run("join") { node.acceptGroupInvite(g) } }
    }

    private fun rename() {
        val field = EditText(this).apply {
            setText(convo?.name ?: "")
            hint = "Name"
            isSingleLine = true
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
        }
        AlertDialog.Builder(this)
            .setTitle("Name this contact")
            .setMessage("Only you see this name.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val name = field.text.toString().trim()
                if (name.isNotEmpty()) worker.execute { run("rename") { node.setName(device, name) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Compares safety numbers, device by device: an unverified one first. */
    private fun safety() {
        val target = convo?.unverified?.firstOrNull() ?: device
        val many = (convo?.devices?.size ?: 1) > 1
        worker.execute {
            val number = try { node.safetyNumber(target) } catch (e: Exception) { return@execute }
            // Twelve groups of five digits, three per line.
            val pretty = number.filter { it.isDigit() }.chunked(5).chunked(3).joinToString("\n") { it.joinToString("  ") }
            runOnUiThread {
                val view = label(pretty, 20f).apply {
                    typeface = Typeface.MONOSPACE
                    gravity = Gravity.CENTER
                    setTextIsSelectable(true)
                    setPadding(dp(24), dp(16), dp(24), 0)
                }
                AlertDialog.Builder(this)
                    .setTitle(if (many) "Safety number: device ${Threnody.short(target)}" else "Safety number")
                    .setMessage("Compare this with the number on ${convo?.title?.let { "$it's" } ?: "their"} " +
                        (if (many) "device ${Threnody.short(target)} " else "") +
                        "screen, in person or on a call. If they match, no one is intercepting your messages.")
                    .setView(view)
                    .setPositiveButton("They match") { _, _ ->
                        worker.execute {
                            run("verify") { node.markVerified(target) }
                            // More devices to check? Offer the next one.
                            val more = Threnody.conversations(node).firstOrNull { it.key == key }?.unverified?.isNotEmpty() == true
                            if (more) runOnUiThread { safety() }
                        }
                    }
                    .setNegativeButton("Not now", null)
                    .show()
            }
        }
    }

    private fun setApproval(approved: Boolean) {
        val apply = { worker.execute { run(if (approved) "approve" else "revoke") { node.setApproval(device, approved) } } }
        if (approved) return apply()
        AlertDialog.Builder(this)
            .setTitle("Revoke approval?")
            .setMessage("${convo?.title ?: "They"} will no longer be able to find you nearby, relay for you or hold your messages.")
            .setPositiveButton("Revoke") { _, _ -> apply() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Sets this conversation's disappearing timer (any of the choices, or off). */
    private fun disappearing() {
        val choices = Privacy.TIMERS
        worker.execute {
            val g = group
            val current = try {
                if (g != null) node.groupDisappearing(g) else node.disappearing(device)
            } catch (_: Exception) { null }
            val checked = choices.indexOfFirst { it.second == current }
            runOnUiThread {
                AlertDialog.Builder(this)
                    .setTitle("Disappearing messages")
                    .setSingleChoiceItems(choices.map { it.first }.toTypedArray(), checked) { d, i ->
                        d.dismiss()
                        worker.execute {
                            run("timer") {
                                if (g != null) node.setGroupDisappearing(g, choices[i].second)
                                else node.setDisappearing(device, choices[i].second)
                            }
                            runOnUiThread {
                                Toast.makeText(this, "New messages: ${choices[i].first.lowercase()}", Toast.LENGTH_SHORT).show()
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun wifiDirect() {
        if (!WifiDirect.permitted(this)) return requestPermissions(arrayOf(WifiDirect.permission), WIFI_DIRECT)
        WifiDirect.host(applicationContext, node, device)
        Toast.makeText(this, "Setting up Wi-Fi Direct…", Toast.LENGTH_SHORT).show()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (code == WIFI_DIRECT && results.isNotEmpty() && results.all { it == 0 }) wifiDirect()
    }

    /** Runs a node call, reporting failure, then refreshes. */
    private fun run(what: String, f: () -> Unit) {
        try { f() } catch (e: Exception) {
            runOnUiThread { Toast.makeText(this, "Couldn't $what: ${e.message}", Toast.LENGTH_LONG).show() }
        }
        refresh()
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    companion object {
        const val KEY = "key"
        const val DEVICE = "device"
        const val GROUP = "group"
        private const val PICK_FILE = 1
        /** Most photos or files sent at once. */
        private const val MAX_PICK = 30
        private const val WIFI_DIRECT = 2
    }
}
