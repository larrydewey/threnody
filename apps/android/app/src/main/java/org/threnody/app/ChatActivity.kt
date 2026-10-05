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
    /** The anonymous identity this chat belongs to; null for the main one. */
    private var persona: String? = null
    /** They proved they are a contact of our main identity: its name. */
    private var revealedKnown: String? = null
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
        persona = intent.getStringExtra(PERSONA)
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
            node = try {
                Threnody.node(this, persona)
            } catch (e: Exception) {
                runOnUiThread {
                    Toast.makeText(this, e.message, Toast.LENGTH_LONG).show()
                    finish()
                }
                return@execute
            }
            refresh()
            // Not connected: look for them across the internet now.
            if (group == null) try { node.seek(device) } catch (_: Exception) {}
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
        ThrenodyService.clearNotification(this, ThrenodyService.notificationKey(key, persona))
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
                is NodeEvent.Reacted -> e.group == g
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
            is NodeEvent.ProfileChanged -> e.peer
            is NodeEvent.Reacted -> if (e.group == null) e.peer else return false
            is NodeEvent.IdentityRevealed -> e.peer
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
            c = Threnody.conversations(node, persona).firstOrNull { it.key == key || device in it.devices }
            if (c != null) {
                key = c.key
                device = c.device
            }
            history = try { node.history(device, 200u) } catch (_: Exception) { emptyList() }
        }
        revealedKnown = c?.revealed?.let { fp ->
            // Known to the main identity, which is who they revealed themselves to.
            Threnody.start(this).contacts().firstOrNull { it.fingerprint == fp }?.let { it.name ?: Threnody.short(fp) }
        }
        val me = node.deviceFingerprint()
        myDevice = me
        val items = history.map { Item(it) } + synchronized(pending) {
            pending.map { Item(HistoryEntry(atMs = ULong.MAX_VALUE, outgoing = true, device = me, text = it, disappearing = false, file = null, delivered = false, id = 0uL, edited = false, recipients = 0u, deliveredTo = 0u, reactions = emptyList()), sending = true) }
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
            persona?.let { "🎭 as ${Threnody.personaLabels[it] ?: "anonymous"}" },
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
        val revealed = c.revealed
        when {
            // They reached us anonymously and then proved who they are.
            revealed != null && revealedKnown == null -> banner(
                "${c.title} proved they are ${Threnody.short(revealed)}. Add them to talk to them as themselves" +
                    (if (persona != null) " (from your main identity)." else "."),
                *(c.revealedInvite?.let { inv -> arrayOf<Pair<String, () -> Unit>>("Add them" to { addRevealed(inv) }) }
                    ?: emptyArray()),
            )
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
            return label(if (location == null) "📷\nnot available" else "🔒\nSensitive photo\nTap to view", 14f, R.color.on_cover).apply {
                gravity = Gravity.CENTER
                setTypeface(typeface, Typeface.BOLD)
                // Opaque and dark in either theme: covered, without glare.
                background = rounded(color(R.color.cover), dp(12).toFloat()).apply {
                    setStroke(dp(1), color(R.color.cover_edge))
                }
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
                    setPadding(dp(10), dp(4), dp(10), 0)
                })
            }
            time
        } else if (file == null) {
            body.addView(TextView(this).apply {
                text = e.text
                textSize = 16f
                setTextColor(fg)
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
                })
            }
            Formatter.formatShortFileSize(this, run.sumOf { it.entry.file?.size ?: 0uL }.toLong()) + " · " + time +
                if (file.location?.startsWith("content:") == true && !outgoing) " · in Downloads" else ""
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
        reactionChips(e, fg)?.let { body.addView(it, matchWrap) }
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
                if (v is ImageView || v is TextView) v.setOnLongClickListener(longPress)
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
            // Text isn't selectable in the bubble (it would take the long
            // press), so copying is offered here.
            if (e.text.isNotEmpty()) {
                add("Copy text")
                add("Select text")
            }
            if (canEdit) add("Edit")
            add("Delete for me")
            if (mine) add("Delete for everyone")
        }
        fun pick(option: String) {
            if (option == "Edit") return editMessage(e)
            if (option == "Copy text") {
                getSystemService(android.content.ClipboardManager::class.java)
                    ?.setPrimaryClip(android.content.ClipData.newPlainText("message", e.text))
                return
            }
            if (option == "Select text") return selectText(e.text)
            val everyone = option == "Delete for everyone"
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
        // Reactions toggle in place (the panel stays open); actions close it.
        ReactionPanel(
            this,
            mine = e.reactions.filter { it.mine }.map { it.emoji }.toSet(),
            canReact = e.id != 0uL,
            actions = options.map { option -> option to { pick(option) } },
        ) { emoji, add -> react(e, emoji, add) }.show()
    }

    /** Shows a message's text where any part of it can be selected and copied. */
    private fun selectText(text: String) {
        val view = TextView(this).apply {
            this.text = text
            textSize = 16f
            setTextColor(color(R.color.text))
            setTextIsSelectable(true)
            setPadding(dp(24), dp(16), dp(24), dp(8))
        }
        AlertDialog.Builder(this)
            .setView(android.widget.ScrollView(this).apply { addView(view) })
            .setPositiveButton("Done", null)
            .show()
    }

    /** Adds or takes away our reaction (several per message are fine). */
    private fun react(e: HistoryEntry, emoji: String, add: Boolean) {
        val g = group
        worker.execute {
            run("react") {
                val ok = if (g != null) node.reactInGroup(g, e.id, emoji, add) else node.react(device, e.id, emoji, add)
                if (!ok) throw IllegalStateException("that message is gone")
            }
        }
    }

    /** Reactions under a message: one chip per emoji; ours stand out and tap to take back. */
    private fun reactionChips(e: HistoryEntry, fg: Int): View? {
        if (e.reactions.isEmpty()) return null
        val row = android.widget.HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        val chips = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; setPadding(0, dp(4), 0, 0) }
        for (r in e.reactions) {
            chips.addView(TextView(this).apply {
                text = if (r.count > 1u) "${r.emoji} ${r.count}" else r.emoji
                textSize = 14f
                setTextColor(fg)
                setPadding(dp(8), dp(2), dp(8), dp(2))
                background = rounded(color(if (r.mine) R.color.accent else R.color.divider), dp(12).toFloat()).apply {
                    alpha = if (r.mine) 90 else 255
                }
                contentDescription = "${r.emoji}, ${r.count}" + if (r.mine) ", yours: tap to take back" else ", tap to add yours"
                setOnClickListener { react(e, r.emoji, !r.mine) }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginEnd = dp(4) })
        }
        row.addView(chips)
        return row
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
                val location = if (Media.isImage(p.name)) Media.savePrivate(this, p.name, p.data, persona) else p.uri.toString()
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

    private fun open(location: Uri) {
        val uri = FilesProvider.shareable(this, location)
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
            menu.add("Media, files and links").setOnMenuItemClickListener { media(c?.title); true }
            menu.add("Rename").setOnMenuItemClickListener { rename(); true }
            menu.add("Safety number").setOnMenuItemClickListener { safety(); true }
            if (c?.approved == true) {
                menu.add("Revoke approval").setOnMenuItemClickListener { setApproval(false); true }
            } else {
                menu.add("Approve").setOnMenuItemClickListener { setApproval(true); true }
            }
            menu.add("Disappearing messages").setOnMenuItemClickListener { disappearing(); true }
            menu.add("Share your profile…").setOnMenuItemClickListener {
                ProfileUi.share(this@ChatActivity, node, device, c?.title ?: "They", worker,
                    if (persona != null) "This identity's profile" else "Your profile")
                true
            }
            if (persona != null && c != null) {
                menu.add("Reveal who you are…").setOnMenuItemClickListener { reveal(c); true }
            }
            if (c?.connected == true && c.approved) {
                menu.add("Faster link (Wi-Fi Direct)").setOnMenuItemClickListener { wifiDirect(); true }
            }
            if (c != null) {
                menu.add("Clear chat…").setOnMenuItemClickListener {
                    Chats.clear(this@ChatActivity, node, worker, c.title, c.device, null) { refresh() }
                    true
                }
                menu.add("Delete contact…").setOnMenuItemClickListener {
                    Chats.delete(this@ChatActivity, node, worker, c.title, c.device) { finish() }
                    true
                }
            }
            show()
        }
    }

    private fun media(title: String?) {
        startActivity(Intent(this, MediaActivity::class.java)
            .putExtra(DEVICE, device)
            .putExtra(GROUP, group)
            .putExtra(PERSONA, persona)
            .putExtra(MediaActivity.TITLE, title))
    }

    /** Proves to them that this anonymous identity is us. Can't be undone. */
    private fun reveal(c: Conversation) {
        val p = persona ?: return
        AlertDialog.Builder(this)
            .setTitle("Reveal who you are?")
            .setMessage(
                "${c.title} will get proof, signed by your main identity, that this anonymous identity is you, " +
                    "and an invite to reach you. They can show that proof to anyone. This can't be taken back.",
            )
            .setPositiveButton("Reveal") { _, _ ->
                worker.execute {
                    val main = Threnody.start(this)
                    val error = try {
                        val p2 = Threnody.personaNode(p) ?: throw IllegalStateException("anonymous identity is gone")
                        main.revealThrough(p2, device, mainInvite(main))
                        null
                    } catch (e: Exception) {
                        e.message
                    }
                    runOnUiThread {
                        Toast.makeText(this, error?.let { "Couldn't reveal: $it" } ?: "${c.title} now knows who you are",
                            Toast.LENGTH_LONG).show()
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Our main identity's invite, on this device's current address. */
    private fun mainInvite(main: ThrenodyNode): String? {
        val cm = getSystemService(android.net.ConnectivityManager::class.java) ?: return null
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return null
        val ip = props.linkAddresses.map { it.address }.firstOrNull { it is java.net.Inet4Address }?.hostAddress ?: return null
        return main.inviteLink("$ip:${Threnody.listenAddr.substringAfterLast(':')}")
    }

    /** Adds the identity they revealed, as a contact of our main identity. */
    private fun addRevealed(invite: String) {
        worker.execute {
            val error = try { Threnody.start(this).connect(invite); null } catch (e: Exception) { e.message }
            runOnUiThread {
                Toast.makeText(this, error?.let { "Couldn't reach them: $it" } ?: "Added. They're in your conversations.",
                    Toast.LENGTH_LONG).show()
            }
            refresh()
        }
    }

    private fun groupMenu(anchor: View) {
        val g = info ?: return
        PopupMenu(this, anchor).apply {
            menu.add("Media, files and links").setOnMenuItemClickListener { media(g.name); true }
            menu.add("Members").setOnMenuItemClickListener { members(); true }
            if (g.owned) menu.add("Invite contacts").setOnMenuItemClickListener { inviteToGroup(); true }
            menu.add("Disappearing messages").setOnMenuItemClickListener { disappearing(); true }
            menu.add("Clear chat…").setOnMenuItemClickListener {
                Chats.clear(this@ChatActivity, node, worker, g.name, null, g.id) { refresh() }
                true
            }
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
            val candidates = Threnody.conversations(node, persona).filter { c -> c.devices.none { it in g.members } }
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
                            val more = Threnody.conversations(node, persona).firstOrNull { it.key == key }?.unverified?.isNotEmpty() == true
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
        const val PERSONA = "persona"
        private const val PICK_FILE = 1
        /** Most photos or files sent at once. */
        private const val MAX_PICK = 30
        private const val WIFI_DIRECT = 2
    }
}
