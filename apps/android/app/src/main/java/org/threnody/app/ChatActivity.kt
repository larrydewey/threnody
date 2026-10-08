package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Typeface
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.text.Editable
import android.text.SpannableString
import android.text.Spanned
import android.text.InputType
import android.text.TextWatcher
import android.text.format.DateFormat
import android.text.format.Formatter
import android.text.style.ForegroundColorSpan
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
    /** "…" below the messages while the contact is typing. */
    private lateinit var typingBubble: View
    private val typingHandler = Handler(Looper.getMainLooper())
    /** We stopped typing for a while: say so. */
    private val stopTyping = Runnable { sendTyping(false) }
    /** The contact's "typing" went quiet (its "stopped" may be lost). */
    private val peerStopped = Runnable { showTyping(false) }
    /** Messages being sent, shown until history has them. */
    private val pending = mutableListOf<String>()
    private var unsubscribe: (() -> Unit)? = null
    private var myDevice = ""
    private val pickFileLauncher = ActivityResultRegistry.get(this)
    private val viewPhotoLauncher = ActivityResultRegistry.get(this)
    private val annotateLauncher = ActivityResultRegistry.get(this)
    /** Incoming message ids already reported read. */
    private val sentRead = mutableSetOf<ULong>()
    /** On screen (between onStart and onStop): only then are messages read. */
    private var started = false
    /** Search in this chat: the box, its matches (newest first) and the one shown. */
    private lateinit var search: SearchBox
    private var hits: List<HistoryEntry> = emptyList()
    private var hit = -1
    private var terms: List<String> = emptyList()
    /** Bumped per search, so a slow one's results don't replace a newer one's. */
    private var searches = 0
    /** Each message's bubble and texts on screen, by [mark], to scroll to and highlight. */
    private val bubbles = mutableMapOf<String, View>()
    private val texts = mutableMapOf<String, MutableList<Pair<TextView, String>>>()
    /** A message to bring into view once loaded: its [mark] and time. */
    private var jump: Pair<String, ULong>? = null
    /**
     * The search result the chat was opened at: the first search selects
     * it. Kept apart from `jump`, which loading may finish with first.
     */
    private var openedAt: String? = null
    /** How many messages are loaded; grows to reach an older match. */
    @Volatile private var historyLimit = HISTORY_PAGE
    /** The oldest loaded message's time, and whether that is the whole history. */
    private var oldestLoaded = 0uL
    private var allLoaded = true

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
            if (group == null) action(R.drawable.ic_call, "Call") { call() }
            action(R.drawable.ic_search, "Search this chat") { search.open() }
            action(R.drawable.ic_more, if (group != null) "Group options" else "Contact options") { more(it) }
        }
        search = SearchBox(this, bar, "Search this chat", stepper = true, query = ::find, step = ::step) { endSearch() }
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
        // Typing indicator bubble (shown when peer is typing).
        typingBubble = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.START
            setPadding(0, dp(3), dp(48), dp(3))
            val bubble = LinearLayout(this@ChatActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                background = rounded(color(R.color.bubble_in), dp(16).toFloat())
                setPadding(dp(12), dp(8), dp(12), dp(8))
                addView(TextView(this@ChatActivity).apply {
                    text = "…"
                    textSize = 16f
                    setTextColor(color(R.color.muted))
                })
            }
            addView(bubble, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        typingBubble.visibility = View.GONE
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
        // Keep the newest message in view when the keyboard opens (unless
        // a search match is).
        scroll.addOnLayoutChangeListener { _, _, _, _, b, _, _, _, ob -> if (b < ob && hit < 0) toBottom() }

        // Opened from a search result: that message, with the search open on it.
        val jumpAt = intent.getLongExtra(JUMP_AT_MS, 0L)
        if (jumpAt > 0) {
            jump = mark(jumpAt.toULong(), intent.getStringExtra(JUMP_DEVICE) ?: "") to jumpAt.toULong()
            openedAt = jump?.first
            intent.getStringExtra(JUMP_QUERY)?.takeIf { it.isNotBlank() }?.let { q ->
                search.open(typing = false)
                search.field.setText(q)
            }
        }

        Threading.background {
            node = try {
                Threnody.node(this, persona)
            } catch (e: Exception) {
                runOnUiThread {
                    Toast.makeText(this, e.message, Toast.LENGTH_LONG).show()
                    finish()
                }
                return@background
            }
            refresh()
            // Not connected: look for them across the internet now.
            if (group == null) try { node.seek(device) } catch (e: Exception) {
                Threnody.say("! seek: ${e.message}")
            }
        }
    }

    private fun composeBar(): LinearLayout {
        compose = ComposeField(this, ::received).apply {
            hint = "Message"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 6)
            background = rounded(color(R.color.field), dp(22).toFloat())
            setPadding(dp(16), dp(10), dp(16), dp(10))
            setTextColor(color(R.color.text))
            setHintTextColor(color(R.color.muted))
            addTextChangedListener(object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                override fun afterTextChanged(s: Editable?) {
                    typingHandler.removeCallbacks(stopTyping)
                    if (s.toString().isBlank()) return sendTyping(false)
                    sendTyping(true)
                    typingHandler.postDelayed(stopTyping, TYPING_IDLE_MS)
                }
            })
        }
        fun button(res: Int, label: String, tint: Int, onClick: (View) -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(color(tint))
            contentDescription = label
            tooltipText = label
            background = ripple(borderless = true)
            layoutParams = LinearLayout.LayoutParams(dp(Design.touchTargetMin), dp(Design.touchTargetMin))
            setOnClickListener { v -> Design.mediumHaptic(v); onClick(v) }
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.BOTTOM
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(4), dp(6), dp(4), dp(6))
            addView(button(R.drawable.ic_attach, "Send photos or files", R.color.muted) { attach(it) }, LinearLayout.LayoutParams(dp(48), dp(48)))
            addView(button(R.drawable.ic_emoji, "Emoji", R.color.muted) {
                EmojiPicker(this@ChatActivity, "Emoji", stay = true) { e ->
                    val at = compose.selectionStart.coerceAtLeast(0)
                    compose.text.replace(at, compose.selectionEnd.coerceAtLeast(at), e)
                }.show()
            }, LinearLayout.LayoutParams(dp(44), dp(48)))
            addView(button(R.drawable.ic_gif, "GIF", R.color.muted) { gifs() }, LinearLayout.LayoutParams(dp(44), dp(48)))
            addView(compose, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { bottomMargin = dp(2) })
            addView(button(R.drawable.ic_send, "Send", R.color.accent) { send() }, LinearLayout.LayoutParams(dp(48), dp(48)))
        }
    }

    /** Whether the contact was last told we are typing, and when. */
    private var typingSent = false
    private var typingSentAt = 0L

    /**
     * Tells the contact (not a group) we started or stopped typing: on a
     * change, and again now and then while typing goes on.
     */
    private fun sendTyping(active: Boolean) {
        if (group != null || !::node.isInitialized) return
        val now = android.os.SystemClock.elapsedRealtime()
        if (typingSent == active && !(active && now - typingSentAt > TYPING_AGAIN_MS)) return
        if (active && !Privacy.sendTyping(this)) return
        typingSent = active
        typingSentAt = now
        val d = device
        Threading.background { try { node.setTyping(d, active) } catch (e: Exception) { Threnody.say("! setTyping: ${e.message}") } }
    }

    /** Shows or hides "…" for the contact typing. */
    private fun showTyping(active: Boolean) {
        if (group != null) return
        typingHandler.removeCallbacks(peerStopped)
        if (active) typingHandler.postDelayed(peerStopped, PEER_TYPING_MS)
        val was = typingBubble.visibility == View.VISIBLE
        typingBubble.visibility = if (active) View.VISIBLE else View.GONE
        if (active && !was && !scroll.canScrollVertically(1)) toBottom()
    }

    override fun onStart() {
        super.onStart()
        started = true
        Threnody.visible++
        Threnody.visibleChat = setOf(key, device) + (convo?.devices ?: emptyList())
        ThrenodyService.clearNotification(this, ThrenodyService.notificationKey(key, persona))
        unsubscribe = Threnody.subscribe { e ->
            if (!concerns(e)) return@subscribe
            if (e is NodeEvent.Typing) {
                runOnUiThread { showTyping(e.active) }
                return@subscribe
            }
            // A message from them ends their typing.
            if (e is NodeEvent.Message) runOnUiThread { showTyping(false) }
            when (e) {
                is NodeEvent.CredentialPresented -> runOnUiThread {
                    CredentialUi.showProof(this, node, e.peer, e.issuer, e.schema, e.attributes, e.pseudonym)
                }
                is NodeEvent.CredentialReceived -> runOnUiThread {
                    Toast.makeText(this, "Got a credential (${e.schema}). Menu → Credentials lists it.", Toast.LENGTH_LONG).show()
                }
                is NodeEvent.CredentialFailed -> runOnUiThread {
                    Toast.makeText(this, "Credential: ${e.reason}", Toast.LENGTH_LONG).show()
                }
                else -> {}
            }
            refreshSoon()
        }
        if (::node.isInitialized) refreshSoon()
    }

    override fun onStop() {
        started = false
        typingHandler.removeCallbacks(stopTyping)
        sendTyping(false)
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
            is NodeEvent.Typing -> e.peer
            is NodeEvent.Read -> e.peer
            is NodeEvent.CredentialOffered -> e.offer.peer
            is NodeEvent.CredentialAsked -> e.ask.peer
            is NodeEvent.CredentialReceived -> e.peer
            is NodeEvent.CredentialPresented -> e.peer
            is NodeEvent.CredentialFailed -> e.peer
            is NodeEvent.AccountChanged -> return true
            else -> return false
        }
        // A device we haven't seen yet may belong to this account.
        return peer == device || convo?.devices?.contains(peer) == true ||
            (::node.isInitialized && Threnody.key(node.contacts(), peer) == key)
    }

    /** Reloads the conversation's state and messages (on the worker thread). */
    /** A refresh is queued and hasn't started yet. */
    private val refreshQueued = java.util.concurrent.atomic.AtomicBoolean(false)
    /** Counts refreshes, so only the newest one is drawn. */
    private val refreshGen = java.util.concurrent.atomic.AtomicInteger(0)

    /**
     * Reloads on the worker thread, once for however many asks arrive
     * meanwhile: a burst of events (a backlog of messages arriving) would
     * otherwise redraw everything once per message.
     */
    private fun refreshSoon() {
        if (refreshQueued.compareAndSet(false, true)) Threading.background {
            refreshQueued.set(false)
            refresh()
        }
    }

    private fun refresh() {
        val gen = refreshGen.incrementAndGet()
        val g = group
        val history: List<HistoryEntry>
        var c: Conversation? = null
        var gi: GroupInfo? = null
        val limit = historyLimit
        if (g != null) {
            gi = node.groups().firstOrNull { it.id == g }
            history = try { node.groupHistory(g, limit) } catch (_: Exception) { emptyList() }
        } else {
            // The key moves from device to account once the account is known.
            c = Threnody.conversations(node, persona).firstOrNull { it.key == key || device in it.devices }
            if (c != null) {
                key = c.key
                device = c.device
            }
            history = try { node.history(device, limit) } catch (_: Exception) { emptyList() }
        }
        revealedKnown = c?.revealed?.let { fp ->
            // Known to the main identity, which is who they revealed themselves to.
            Threnody.start(this).contacts().firstOrNull { it.fingerprint == fp }?.let { it.name ?: Threnody.short(fp) }
        }
        val me = node.deviceFingerprint()
        myDevice = me
        val items = history.map { Item(it) } + synchronized(pending) {
            pending.map { Item(HistoryEntry(atMs = ULong.MAX_VALUE, outgoing = true, device = me, text = it, disappearing = false, file = null, delivered = false, read = false, id = 0uL, edited = false, recipients = 0u, deliveredTo = 0u, reactions = emptyList()), sending = true) }
        }
        val names = if (g != null) history.map { it.device }.distinct().associateWith { Threnody.nameOf(node, it) } else emptyMap()
        // Credential offers and requests from this contact, waiting for an answer.
        val mine = c?.devices ?: listOf(device)
        val offers = if (g == null) node.credentialOffers().filter { it.peer in mine } else emptyList()
        val asks = if (g == null) node.credentialAsks().filter { it.peer in mine } else emptyList()
        runOnUiThread {
            // A newer refresh will draw instead.
            if (gen != refreshGen.get()) return@runOnUiThread
            convo = c
            info = gi
            if (Threnody.visibleChat.isNotEmpty()) {
                Threnody.visibleChat = setOf(key, device) + (c?.devices ?: emptyList())
            }
            if (g != null) groupHeader(gi) else header(c)
            for (o in offers) banner(
                "${c?.title ?: "They"} offers you a credential (${o.schema}).",
                "Review" to { CredentialUi.answerOffer(this, node, o) { refreshSoon() } },
            )
            for (q in asks) banner(
                "${c?.title ?: "They"} asks you to prove " +
                    (if (q.keys.isEmpty()) "you hold a credential (${q.schema})." else "${q.keys.joinToString(", ")} (${q.schema})."),
                "Review" to { CredentialUi.answerAsk(this, node, q) { refreshSoon() } },
            )
            oldestLoaded = history.firstOrNull()?.atMs ?: 0uL
            allLoaded = history.size.toUInt() < limit
            show(items, names)
            jump?.let { (k, at) ->
                when {
                    k in bubbles -> { jump = null; reveal(k) }
                    // Older than what's loaded: load back to it, then look again.
                    !allLoaded && oldestLoaded > at && historyLimit < MAX_HISTORY -> loadUntil(at)
                    else -> jump = null
                }
            }
            // Incoming messages now on screen are read (1:1 chats only).
            if (g == null && started && Privacy.sendReadReceipts(this)) {
                val ids = synchronized(sentRead) {
                    items.filter { !it.entry.outgoing && it.entry.id != 0uL && it.entry.id !in sentRead }.map { it.entry.id }
                }
                if (ids.isNotEmpty()) Threading.background {
                    // Without a session now, they're reported on a later refresh.
                    try {
                        node.reportRead(device, ids)
                        synchronized(sentRead) { sentRead.addAll(ids) }
                    } catch (e: Exception) {
                        Threnody.say("! reportRead: ${e.message}")
                    }
                }
            }
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
        bubbles.clear()
        texts.clear()
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
            val view = bubble(run, sender)
            messages.addView(view)
            for (item in run) bubbles[mark(item.entry)] = view
        }
        messages.addView(typingBubble, matchWrap)
        if (atEnd && hit < 0 && jump == null || items.lastOrNull()?.sending == true) toBottom()
    }

    /** Identifies a message across reloads and search results. */
    private fun mark(e: HistoryEntry) = mark(e.atMs, e.device)
    private fun mark(atMs: ULong, device: String) = "$atMs/$device"

    /** Searches this chat (on the worker thread); shows the newest match. */
    private fun find(q: String) {
        val t = Search.terms(q)
        val n = ++searches
        if (t.isEmpty()) return found(n, t, emptyList())
        val g = group
        val d = device
        Threading.background {
            if (!::node.isInitialized) return@background
            val list = try {
                if (g != null) node.searchGroupMessages(g, q, SEARCH_LIMIT) else node.searchMessages(d, q, SEARCH_LIMIT)
            } catch (e: Exception) {
                Threnody.say("! search: ${e.message}")
                emptyList()
            }
            runOnUiThread { found(n, t, list) }
        }
    }

    private fun found(n: Int, t: List<String>, list: List<HistoryEntry>) {
        if (n != searches || !search.isOpen) return
        terms = t
        hits = list
        // Opened from a search result: start at that one.
        val wanted = openedAt ?: jump?.first
        openedAt = null
        hit = list.indexOfFirst { mark(it) == wanted }.takeIf { it >= 0 } ?: if (list.isEmpty()) -1 else 0
        search.setCount(hit, list.size)
        paint()
        if (hit >= 0) goTo(list[hit])
    }

    /** To an older (+1) or newer (-1) match. */
    private fun step(by: Int) {
        val i = hit + by
        if (i !in hits.indices) return
        hit = i
        search.setCount(hit, hits.size)
        paint()
        goTo(hits[i])
    }

    private fun endSearch() {
        searches++
        hits = emptyList()
        hit = -1
        terms = emptyList()
        jump = null
        paint()
    }

    /** Scrolls to a match, loading older history first if it isn't loaded. */
    private fun goTo(e: HistoryEntry) {
        val k = mark(e)
        if (k in bubbles) {
            jump = null
            return reveal(k)
        }
        jump = k to e.atMs
        if (!allLoaded && oldestLoaded > e.atMs) loadUntil(e.atMs)
    }

    /** Loads more history, a page at a time, until it reaches `at` (then refreshes). */
    private fun loadUntil(at: ULong) {
        val g = group
        val d = device
        Threading.background {
            var limit = historyLimit
            while (limit < MAX_HISTORY) {
                limit = minOf(limit * 2u, MAX_HISTORY)
                val h = try { if (g != null) node.groupHistory(g, limit) else node.history(d, limit) } catch (_: Exception) { break }
                if (h.size.toUInt() < limit || (h.firstOrNull()?.atMs ?: 0uL) <= at) break
            }
            historyLimit = limit
            refresh()
        }
    }

    /** Marks the search terms in every bubble's text; the current match stands out. */
    private fun paint() {
        for ((k, views) in texts) for ((v, raw) in views) v.text = marked(k, raw)
    }

    private fun marked(k: String, raw: String): CharSequence =
        if (terms.isEmpty()) raw else Search.highlight(raw, terms, k == hits.getOrNull(hit)?.let(::mark))

    /** A bubble's text, kept so search can mark matches in it. */
    private fun TextView.searchable(e: HistoryEntry, raw: String): TextView {
        val k = mark(e)
        texts.getOrPut(k) { mutableListOf() }.add(this to raw)
        text = marked(k, raw)
        return this
    }

    /** Scrolls a message into view (a third of the way down) and flashes it. */
    private fun reveal(k: String) {
        val row = bubbles[k] ?: return
        val go = {
            scroll.smoothScrollTo(0, maxOf(0, row.top - scroll.height / 3))
            flash(row)
        }
        if (row.isLaidOut && !row.isLayoutRequested) row.post(go)
        else row.addOnLayoutChangeListener(object : View.OnLayoutChangeListener {
            override fun onLayoutChange(v: View, l: Int, t: Int, r: Int, b: Int, ol: Int, ot: Int, oldR: Int, ob: Int) {
                v.removeOnLayoutChangeListener(this)
                v.post(go)
            }
        })
    }

    /** A brief tint behind a message, to show which one a search landed on. */
    private fun flash(v: View) {
        val tint = android.graphics.drawable.ColorDrawable(color(R.color.accent)).apply { alpha = 0 }
        v.background = tint
        android.animation.ValueAnimator.ofInt(0, 70, 0).apply {
            duration = 1_200L
            startDelay = Design.durationNormal.toLong()
            addUpdateListener { tint.alpha = it.animatedValue as Int }
            start()
        }
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
            // For a result: a photo edited there comes back here to send.
            viewPhotoLauncher.launch(
                Intent(this, ImageActivity::class.java)
                    .putExtra(ImageActivity.LOCATION, location)
                    .putExtra(ImageActivity.NAME, f.name)
                    .putExtra(ImageActivity.CAPTION, e.text)
            ) { resultCode, data ->
                val path = data?.getStringExtra(AnnotateActivity.RESULT_PATH)
                if (resultCode == RESULT_OK && path != null) sendEdited(path, data.getStringExtra(ImageActivity.NAME) ?: "photo.jpg")
            }
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
        val still = { Media.thumbnail(this, location, width) { b -> runOnUiThread { view.setImageBitmap(b); view.minimumHeight = 0 } } }
        if (!Media.isGif(f.name)) still()
        else Media.animated(this, location, width) { d ->
            if (d == null) return@animated still()
            runOnUiThread {
                view.setImageDrawable(d)
                view.minimumHeight = 0
                (d as android.graphics.drawable.AnimatedImageDrawable).start()
            }
        }
        if (Media.isGif(f.name)) view.contentDescription = e.text.ifBlank { "GIF" }
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
                    searchable(e, e.text)
                    textSize = 16f
                    setTextColor(fg)
                    setPadding(dp(10), dp(4), dp(10), 0)
                })
            }
            time
        } else if (file == null) {
            body.addView(TextView(this).apply {
                searchable(e, e.text)
                textSize = 16f
                setTextColor(fg)
            })
            time
        } else {
            for (r in run) {
                val f = r.entry.file ?: continue
                body.addView(TextView(this).apply {
                    searchable(r.entry, (if (f.sensitive) "📎 Sensitive file: " else "📎 ") + f.name)
                    textSize = 16f
                    setTextColor(fg)
                    setTypeface(typeface, Typeface.BOLD)
                    val uri = f.location?.let(Uri::parse)
                    if (uri != null) setOnClickListener { open(uri) }
                })
            }
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    searchable(e, e.text)
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
            val metaText = meta + (if (e.edited) " · edited" else "") + (if (e.disappearing) " · ⏱" else "") + tick
            contentDescription = metaText.replace("✓✓", "delivered").replace(" ✓ ", " delivered to ").replace("✓", "sent")
            textSize = 11f
            setTextColor(fg)
            alpha = 0.7f
            gravity = Gravity.END
            // Color the ticks blue when message is read (Signal-style).
            if (e.read && e.outgoing && e.delivered) {
                val spannable = SpannableString(metaText)
                val idx = metaText.lastIndexOf("✓")
                if (idx >= 0) {
                    spannable.setSpan(ForegroundColorSpan(color(R.color.accent)), idx, metaText.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
                }
                text = spannable
            }
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
            Threading.background {
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
        SecureBuilder(this)
            .setView(android.widget.ScrollView(this).apply { addView(view) })
            .setPositiveButton("Done", null)
            .show()
    }

    /** Adds or takes away our reaction (several per message are fine). */
    private fun react(e: HistoryEntry, emoji: String, add: Boolean) {
        val g = group
        Threading.background {
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
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 10)
        }
        SecureBuilder(this)
            .setTitle("Edit message")
            .setMessage("They see it marked as edited, if they're running a current version.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val body = field.text.toString().trim()
                if (body.isEmpty() || body == e.text) return@setPositiveButton
                Threading.background { run("edit") { node.editMessage(device, e.id, body) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun time(ms: Long) = DateFormat.getTimeFormat(this).format(Date(ms))

    private fun send() {
        val text = compose.text.toString().trim()
        if (text.isEmpty() || !::node.isInitialized) return
        typingHandler.removeCallbacks(stopTyping)
        compose.setText("") // also says we stopped typing
        Threnody.touch()
        synchronized(pending) { pending.add(text) }
        refreshSoon()
        Threading.background {
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

    /**
     * Photos, files, or (`gifs`) only the GIFs already on this phone: no
     * GIF service is asked. The keyboard's GIF search works too; see
     * [ComposeField].
     */
    private fun pick(photos: Boolean, gifs: Boolean = false) {
        val intent = if (photos && android.os.Build.VERSION.SDK_INT >= 33) {
            // The system photo picker: no storage permission needed.
            Intent(android.provider.MediaStore.ACTION_PICK_IMAGES)
                .putExtra(android.provider.MediaStore.EXTRA_PICK_IMAGES_MAX, MAX_PICK)
                .apply { if (gifs) type = "image/gif" }
        } else {
            Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                .setType(if (gifs) "image/gif" else if (photos) "image/*" else "*/*")
                .putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
        pickFileLauncher.launch(intent) { resultCode, data ->
            if (resultCode != RESULT_OK || data == null) return@launch
            val clip = data.clipData
            val uris = if (clip != null) (0 until clip.itemCount).map { clip.getItemAt(it).uri } else listOfNotNull(data.data)
            if (uris.isEmpty()) return@launch
            Threading.background {
                try {
                    val max = node.maxFileSize().toLong()
                    val picked = uris.take(MAX_PICK).map { uri ->
                        // Keep access so a sent file can be opened from the chat later.
                        try { contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) } catch (e: Exception) {
                            Threnody.say("! persist uri permission: ${e.message}")
                        }
                        val (shown, size) = contentResolver.query(uri, null, null, null, null)?.use { c ->
                            c.moveToFirst()
                            c.getString(c.getColumnIndexOrThrow(OpenableColumns.DISPLAY_NAME)) to
                                c.getLong(c.getColumnIndexOrThrow(OpenableColumns.SIZE))
                        } ?: ("file" to -1L)
                        // A GIF must be named one to be shown animated.
                        val gif = contentResolver.getType(uri) == "image/gif" && !Media.isGif(shown)
                        val name = if (gif) shown.substringBeforeLast('.') + ".gif" else shown
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
                    runOnUiThread { Toast.makeText(this@ChatActivity, "Couldn't send: ${e.message}", Toast.LENGTH_LONG).show() }
                }
            }
        }
    }

    /**
     * A GIF or sticker from the keyboard: into the send sheet, like a
     * picked photo.
     */
    private fun received(uri: Uri, mime: String, release: () -> Unit) {
        Threading.background {
            val picked = try {
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                    ?: throw IllegalArgumentException("unreadable")
                val ext = mime.substringAfter('/')
                val (n, d) = Media.prepare(this, "${if (ext == "gif") "gif" else "sticker"}-${System.currentTimeMillis()}.$ext", bytes)
                if (d.size > node.maxFileSize().toLong()) throw IllegalArgumentException("too large to send")
                Picked(n, d, uri)
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't add it: ${e.message}", Toast.LENGTH_LONG).show() }
                null
            } finally {
                release()
            }
            if (picked != null) runOnUiThread { sendSheet(listOf(picked)) }
        }
    }

    /**
     * GIPHY's GIFs once the user has agreed to GIPHY seeing their searches;
     * until then, or without an API key, the GIFs on this phone.
     */
    private fun gifs() {
        val phone = { pick(photos = true, gifs = true) }
        if (!Privacy.giphy(this)) {
            SecureBuilder(this)
                .setTitle("Search GIFs with GIPHY?")
                .setMessage("GIPHY will see what you search for and this phone's IP address. " +
                    "It won't see who you send GIFs to, and your contacts' phones never contact it. " +
                    "You can turn this off in Settings.")
                .setPositiveButton("Use GIPHY") { _, _ -> Privacy.setGiphy(this, true); gifs() }
                .setNegativeButton("GIFs on this phone") { _, _ -> phone() }
                .show()
            return
        }
        if (Giphy.key(this).isBlank()) return giphyKey { gifs() }
        GifPicker(this, phone) { g -> giphy(g) }.show()
    }

    /** Asks for a GIPHY API key (from developers.giphy.com), as this build has none. */
    private fun giphyKey(then: () -> Unit) {
        val field = EditText(this).apply {
            hint = "API key"
            wrapping(newlines = false, max = 1)
        }
        SecureBuilder(this)
            .setTitle("GIPHY API key")
            .setMessage("This copy of Threnody was built without one. Get a free key at developers.giphy.com.")
            .setView(android.widget.FrameLayout(this).apply {
                setPadding(dp(20), 0, dp(20), 0)
                addView(field)
            })
            .setPositiveButton("Save") { _, _ ->
                if (field.text.isNotBlank()) { Giphy.setKey(this, field.text.toString()); then() }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Downloads a GIF from GIPHY into the send sheet, like a picked photo. */
    private fun giphy(g: Giphy.Gif) {
        Toast.makeText(this, "Getting the GIF…", Toast.LENGTH_SHORT).show()
        Giphy.net.execute {
            try {
                val max = node.maxFileSize().toLong()
                val bytes = Giphy.get(g.full, max)
                val picked = Picked("gif-${g.id.ifBlank { System.currentTimeMillis().toString() }}.gif", bytes, Uri.EMPTY)
                runOnUiThread { sendSheet(listOf(picked)) }
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't get the GIF: ${e.message}", Toast.LENGTH_LONG).show() }
            }
        }
    }

    /** A picked file, read and ready to send. */
    private class Picked(val name: String, val data: ByteArray, val uri: Uri)

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (!ActivityResultRegistry.dispatch(this, requestCode, resultCode, data)) {
            super.onActivityResult(requestCode, resultCode, data)
        }
    }

    /** Previews what's about to go, with a caption and the sensitive choice. */
    private fun sendSheet(picked: List<Picked>) {
        val images = picked.count { Media.isImage(it.name) }
        val strip = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for ((i, p) in picked.withIndex()) {
            val cell = if (Media.isGif(p.name)) {
                ImageView(this).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                    clipToOutline = true
                    contentDescription = "GIF"
                    Media.animated({ android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(p.data)) }, dp(200)) { d ->
                        runOnUiThread { setImageDrawable(d); (d as? android.graphics.drawable.AnimatedImageDrawable)?.start() }
                    }
                }
            } else if (Media.isImage(p.name)) {
                // The photo, with an Edit button on it: drawing and text.
                android.widget.FrameLayout(this).apply {
                    contentDescription = "${p.name}. Edit: draw or add text"
                    setOnClickListener { annotate(picked, i) }
                    addView(ImageView(this@ChatActivity).apply {
                        scaleType = ImageView.ScaleType.CENTER_CROP
                        background = rounded(color(R.color.surface), dp(8).toFloat())
                        clipToOutline = true
                        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                        Threading.background {
                            val b = Media.decode(android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(p.data)), dp(200))
                            runOnUiThread { setImageBitmap(b) }
                        }
                    }, MATCH_PARENT, MATCH_PARENT)
                    addView(TextView(this@ChatActivity).apply {
                        text = "✏️ Edit"
                        textSize = 12f
                        setTypeface(typeface, Typeface.BOLD)
                        setTextColor(android.graphics.Color.WHITE)
                        background = rounded(android.graphics.Color.argb(170, 0, 0, 0), dp(12).toFloat())
                        setPadding(dp(8), dp(3), dp(8), dp(3))
                        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                    }, android.widget.FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.BOTTOM or Gravity.END).apply {
                        setMargins(dp(4), dp(4), dp(4), dp(4))
                    })
                }
            } else {
                label("📎\n${p.name}", 12f, R.color.text).apply {
                    gravity = Gravity.CENTER
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                }
            }
            strip.addView(cell, LinearLayout.LayoutParams(dp(104), dp(104)).apply { marginEnd = dp(6) })
        }
        val caption = EditText(this).apply {
            hint = "Add a message"
            setText(compose.text)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 6)
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
        val gifs = picked.count { Media.isGif(it.name) }
        val what = when {
            gifs == picked.size -> if (gifs == 1) "1 GIF" else "$gifs GIFs"
            images == picked.size -> if (images == 1) "1 photo" else "$images photos"
            picked.size == 1 -> "1 file"
            else -> "${picked.size} files"
        }
        sheet = Sheet(picked, caption, sensitive, SecureBuilder(this)
            .setTitle("Send $what")
            .setView(android.widget.ScrollView(this).apply { addView(body) })
            .setPositiveButton("Send") { _, _ ->
                compose.setText("")
                sendFiles(picked, caption.text.toString().trim(), sensitive.isChecked)
            }
            .setNegativeButton("Cancel", null)
            .setOnDismissListener { if (sheet?.annotating != true) sheet = null }
            .show())
    }

    /** The open send sheet, kept while one of its photos is drawn on. */
    private class Sheet(
        val picked: List<Picked>,
        val caption: EditText,
        val sensitive: android.widget.CheckBox,
        val dialog: AlertDialog,
        var annotating: Boolean = false,
    )
    private var sheet: Sheet? = null

    /** Opens picked photo `i` for editing; the sheet comes back with the result. */
    private fun annotate(picked: List<Picked>, i: Int) {
        val s = sheet ?: return
        val p = picked[i]
        Threading.background {
            val f = java.io.File(java.io.File(cacheDir, AnnotateActivity.DIR).apply { mkdirs() }, "source-$i")
            try { f.writeBytes(p.data) } catch (_: Exception) { return@background }
            runOnUiThread {
                s.annotating = true
                s.dialog.dismiss()
                annotateLauncher.launch(
                    Intent(this, AnnotateActivity::class.java)
                        .putExtra(AnnotateActivity.LOCATION, Uri.fromFile(f).toString())
                        .putExtra(AnnotateActivity.NAME, p.name)
                        .putExtra(AnnotateActivity.INDEX, i)
                ) { resultCode, data ->
                    annotated(if (resultCode == RESULT_OK) data else null)
                }
            }
        }
    }

    /** A photo edited from the viewer: ready to send, in the send sheet. */
    private fun sendEdited(path: String, name: String) {
        Threading.background {
            val f = java.io.File(path)
            val bytes = try { f.readBytes() } catch (_: Exception) { null }
            java.io.File(cacheDir, AnnotateActivity.DIR).deleteRecursively()
            if (bytes != null) runOnUiThread {
                sendSheet(listOf(Picked(name.substringBeforeLast('.') + "-edited.jpg", bytes, Uri.fromFile(f))))
            }
        }
    }

    /** An edited photo came back: it replaces the picked one in the sheet. */
    private fun annotated(data: Intent?) {
        val s = sheet ?: return
        val i = data?.getIntExtra(AnnotateActivity.INDEX, -1) ?: -1
        val path = data?.getStringExtra(AnnotateActivity.RESULT_PATH)
        compose.setText(s.caption.text)
        val sensitive = s.sensitive.isChecked
        Threading.background {
            val bytes = if (path == null || i !in s.picked.indices) null
                else try { java.io.File(path).readBytes() } catch (_: Exception) { null }
            java.io.File(cacheDir, AnnotateActivity.DIR).deleteRecursively()
            runOnUiThread {
                val picked = if (bytes == null) s.picked else s.picked.toMutableList().also {
                    val old = it[i]
                    it[i] = Picked(old.name.substringBeforeLast('.') + ".jpg", bytes, old.uri)
                }
                sendSheet(picked)
                sheet?.sensitive?.isChecked = sensitive
            }
        }
    }

    private fun sendFiles(picked: List<Picked>, caption: String, sensitive: Boolean) {
        val album = if (picked.size > 1) uniffi.threnody_ffi.albumId() else 0uL
        Threading.background {
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
            menu.add("Offer a credential…").setOnMenuItemClickListener {
                CredentialUi.offer(this@ChatActivity, node, device, c?.title ?: "They"); true
            }
            menu.add("Ask for a credential…").setOnMenuItemClickListener {
                CredentialUi.ask(this@ChatActivity, node, device, c?.title ?: "They"); true
            }
            menu.add("Share your profile…").setOnMenuItemClickListener {
                ProfileUi.share(this@ChatActivity, node, device, c?.title ?: "They",
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
                    Chats.clear(this@ChatActivity, node, c.title, c.device, null) { refresh() }
                    true
                }
                menu.add("Delete contact…").setOnMenuItemClickListener {
                    Chats.delete(this@ChatActivity, node, c.title, c.device) { finish() }
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
        SecureBuilder(this)
            .setTitle("Reveal who you are?")
            .setMessage(
                "${c.title} will get proof, signed by your main identity, that this anonymous identity is you, " +
                    "and an invite to reach you. They can show that proof to anyone. This can't be taken back.",
            )
            .setPositiveButton("Reveal") { _, _ ->
                Threading.background {
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
        Threading.background {
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
                Chats.clear(this@ChatActivity, node, g.name, null, g.id) { refresh() }
                true
            }
            menu.add(if (g.owned) "Delete group" else "Leave group").setOnMenuItemClickListener { leaveGroup(); true }
            show()
        }
    }

    /** The members, by name; the owner can remove them. */
    private fun members() {
        val g = info ?: return
        Threading.background {
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
                val d = SecureBuilder(this)
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
        SecureBuilder(this)
            .setTitle("Remove $name?")
            .setMessage("They stop receiving new messages in ${info?.name}. You can invite them again later.")
            .setPositiveButton("Remove") { _, _ -> Threading.background { run("remove") { node.removeFromGroup(g, fp) } } }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun inviteToGroup() {
        val g = info ?: return
        Threading.background {
            val candidates = Threnody.conversations(node, persona).filter { c -> c.devices.none { it in g.members } }
            runOnUiThread {
                if (candidates.isEmpty()) {
                    Toast.makeText(this, "All your contacts are already in this group", Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                val chosen = BooleanArray(candidates.size)
                SecureBuilder(this)
                    .setTitle("Invite to ${g.name}")
                    .setMultiChoiceItems(candidates.map { it.title }.toTypedArray(), chosen) { _, i, on -> chosen[i] = on }
                    .setPositiveButton("Invite") { _, _ ->
                        val picked = candidates.filterIndexed { i, _ -> chosen[i] }
                        Threading.background {
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
        SecureBuilder(this)
            .setTitle(if (g.owned) "Delete ${g.name}?" else "Leave ${g.name}?")
            .setMessage(
                if (g.owned) "Everyone is removed and the group ends. Its history stays on your devices."
                else "You stop receiving its messages. Its history stays on your devices; the owner can invite you again.",
            )
            .setPositiveButton(if (g.owned) "Delete" else "Leave") { _, _ ->
                Threading.background {
                    run(if (g.owned) "delete the group" else "leave") { node.leaveGroup(g.id) }
                    runOnUiThread { finish() }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Answers a message request: accept, block or delete. */
    private fun request(what: String) {
        Threading.background {
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
        Threading.background {
            run("decline") { node.declineGroupInvite(g) }
            runOnUiThread { finish() }
        }
    }

    private fun joinGroup() {
        val g = group ?: return
        Threading.background { run("join") { node.acceptGroupInvite(g) } }
    }

    private fun rename() {
        val field = EditText(this).apply {
            setText(convo?.name ?: "")
            hint = "Name"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
            wrapping(newlines = false, max = 3)
        }
        SecureBuilder(this)
            .setTitle("Name this contact")
            .setMessage("Only you see this name.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val name = field.text.toString().trim()
                if (name.isNotEmpty()) Threading.background { run("rename") { node.setName(device, name) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Compares safety numbers, device by device: an unverified one first. */
    private fun safety() {
        val target = convo?.unverified?.firstOrNull() ?: device
        val many = (convo?.devices?.size ?: 1) > 1
        Threading.background {
            val number = try { node.safetyNumber(target) } catch (e: Exception) { return@background }
            // Twelve groups of five digits, three per line.
            val pretty = number.filter { it.isDigit() }.chunked(5).chunked(3).joinToString("\n") { it.joinToString("  ") }
            runOnUiThread {
                val view = label(pretty, 20f).apply {
                    typeface = Typeface.MONOSPACE
                    gravity = Gravity.CENTER
                    setPadding(dp(24), dp(16), dp(24), 0)
                }
                SecureBuilder(this)
                    .setTitle(if (many) "Safety number: device ${Threnody.short(target)}" else "Safety number")
                    .setMessage("Compare this with the number on ${convo?.title?.let { "$it's" } ?: "their"} " +
                        (if (many) "device ${Threnody.short(target)} " else "") +
                        "screen, in person or on a call. If they match, no one is intercepting your messages.")
                    .setView(view)
                    .setPositiveButton("They match") { _, _ ->
                        Threading.background {
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
        val apply = { Threading.background { run(if (approved) "approve" else "revoke") { node.setApproval(device, approved) } } }
        if (approved) return apply()
        SecureBuilder(this)
            .setTitle("Revoke approval?")
            .setMessage("${convo?.title ?: "They"} will no longer be able to find you nearby, relay for you or hold your messages.")
            .setPositiveButton("Revoke") { _, _ -> apply() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Sets this conversation's disappearing timer (any of the choices, or off). */
    private fun disappearing() {
        val choices = Privacy.TIMERS
        Threading.background {
            val g = group
            val current = try {
                if (g != null) node.groupDisappearing(g) else node.disappearing(device)
            } catch (_: Exception) { null }
            val checked = choices.indexOfFirst { it.second == current }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle("Disappearing messages")
                    .setSingleChoiceItems(choices.map { it.first }.toTypedArray(), checked) { d, i ->
                        d.dismiss()
                        Threading.background {
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

    /** Calls this contact (voice). */
    private fun call() {
        if (!::node.isInitialized) return
        Calls.start(this, node, persona, device, convo?.title ?: Threnody.short(device))
    }

    private fun wifiDirect() {
        if (!WifiDirect.permitted(this)) return requestPermissions(arrayOf(WifiDirect.permission), WIFI_DIRECT)
        WifiDirect.host(applicationContext, node, device)
        Toast.makeText(this, "Setting up Wi-Fi Direct…", Toast.LENGTH_SHORT).show()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        Calls.permissionResult(code, results)
        if (code == WIFI_DIRECT && results.isNotEmpty() && results.all { it == 0 }) wifiDirect()
    }

    /** Runs a node call, reporting failure, then refreshes. */
    private fun run(what: String, f: () -> Unit) {
        try { f() } catch (e: Exception) {
            runOnUiThread { Toast.makeText(this, "Couldn't $what: ${e.message}", Toast.LENGTH_LONG).show() }
        }
        refresh()
    }

    // Android 10–12; from 13 the search box takes Back itself.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        if (search.isOpen) search.close() else super.onBackPressed()
    }

    override fun onDestroy() {
        super.onDestroy()
    }

    companion object {
        const val KEY = "key"
        const val DEVICE = "device"
        const val GROUP = "group"
        const val PERSONA = "persona"
        /** Opened from a search result: the message to show (its time and sender device), and the query. */
        const val JUMP_AT_MS = "jump_at_ms"
        const val JUMP_DEVICE = "jump_device"
        const val JUMP_QUERY = "jump_query"
        /** Messages loaded at first, and the most loaded to reach an old match. */
        private const val HISTORY_PAGE = 200u
        private const val MAX_HISTORY = 25_600u
        /** Most matches a chat search lists. */
        private const val SEARCH_LIMIT = 1_000u
        /** Most photos or files sent at once. */
        private const val MAX_PICK = 30
        private const val WIFI_DIRECT = 2
        /** We're taken to have stopped typing after this long without a keystroke. */
        private const val TYPING_IDLE_MS = 5_000L
        /** The contact's "…" goes after this long without word (refreshed while it types). */
        private const val PEER_TYPING_MS = 8_000L
        private const val TYPING_AGAIN_MS = 4_000L
    }
}
