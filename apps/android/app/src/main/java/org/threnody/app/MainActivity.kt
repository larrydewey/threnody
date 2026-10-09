package org.threnody.app

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.os.Bundle
import android.text.InputType
import android.text.format.DateUtils
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.animation.AnimationUtils
import android.view.animation.LayoutAnimationController
import android.widget.Button
import android.widget.EditText
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import android.graphics.drawable.ColorDrawable
import java.net.Inet4Address
import uniffi.threnody_ffi.DeviceInfo
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.ThrenodyNode
import uniffi.threnody_ffi.qrMatrix
import uniffi.threnody_ffi.qrMatrix

/** The conversation list, plus invites, adding contacts and device linking. */
class MainActivity : Activity() {

    private var node: ThrenodyNode? = null
    private lateinit var list: LinearLayout
    private lateinit var scroll: ScrollView
    private lateinit var emptyStateContainer: LinearLayout
    private var unsubscribe: (() -> Unit)? = null
    /** Search across every conversation: while it has a query, the list shows what it found. */
    private lateinit var search: SearchBox
    /** Bumped per search, so a slow one's results don't replace a newer one's. */
    private var searches = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)

        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }

        // Top bar with dynamic color support
        val bar = TopBar(this, null).apply {
            title.text = "Threnody"
            title.setTextAppearance(Design.styleTitleLarge)
            action(R.drawable.ic_search, "Search messages") { search.open() }
            action(R.drawable.ic_qr, "My invite") { showInvite() }
            action(R.drawable.ic_add, "New conversation") { add(it) }
            action(R.drawable.ic_more, "More") { more(it) }
        }
        search = SearchBox(this, bar, "Search messages", stepper = false, query = ::find) {
            refreshSoon()
        }

        // Empty state container (shown when no conversations)
        emptyStateContainer = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            visibility = View.GONE
            setPadding(dp(Design.xl), dp(Design.xxl), dp(Design.xl), dp(Design.xxl))
        }

        // Conversation list
        list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            clipToPadding = false
            setPadding(dp(Design.screenPadding), dp(Design.md), dp(Design.screenPadding), dp(Design.xl))
        }

        // Scroll content container holds both list and empty state
        val scrollContent = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(list, MATCH_PARENT, WRAP_CONTENT)
            addView(emptyStateContainer, MATCH_PARENT, WRAP_CONTENT)
        }

        scroll = ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            addView(scrollContent, MATCH_PARENT, WRAP_CONTENT)
        }

        root.addView(bar, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, scroll)

        ThrenodyService.start(this)
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 3)
        }
        Threading.background {
            try {
                node = Threnody.start(this)
                refresh()
            } catch (e: Exception) {
                runOnUiThread { failed("Couldn't start: ${e.message}") }
            }
        }
        if (savedInstanceState == null) handleLink(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleLink(intent)
    }

    override fun onStart() {
        super.onStart()
        Threnody.visible++
        unsubscribe = Threnody.subscribe { refreshSoon() }
        refreshSoon()
    }

    override fun onStop() {
        Threnody.visible--
        unsubscribe?.invoke()
        super.onStop()
    }

    /** Conversation row data with section info. */
    private data class Row(
        val title: String,
        val avatarKey: String,
        val preview: CharSequence,
        val atMs: Long,
        val connected: Boolean?,
        val open: () -> Unit,
        val manage: (() -> Unit)? = null,
        val section: Section = Section.Contacts,
        val isSectionHeader: Boolean = false,
        val sectionTitle: String? = null,
    ) {
        enum class Section { Requests, Invites, Contacts, Groups, Anonymous }
    }

    /** A refresh is queued and hasn't started yet. */
    private val refreshQueued = java.util.concurrent.atomic.AtomicBoolean(false)
    /** Counts refreshes, so only the newest one is drawn. */
    private val refreshGen = java.util.concurrent.atomic.AtomicInteger(0)

    /**
     * Reloads on the worker thread, once for however many asks arrive
     * meanwhile: a burst of events (a backlog of messages arriving) would
     * otherwise rebuild the list once per message.
     */
    private fun refreshSoon() {
        if (refreshQueued.compareAndSet(false, true)) Threading.background {
            refreshQueued.set(false)
            refresh()
        }
    }

    /** Reloads the list (on the worker thread). */
    private fun refresh() {
        val gen = refreshGen.incrementAndGet()
        val n = node ?: return
        val all = Threnody.conversations(n).filter { !it.blocked }

        val rows = mutableListOf<Row>()

        // Section: Message Requests
        val requests = all.filter { !it.accepted }.mapNotNull { c ->
            val last = try { n.history(c.device, 1u).lastOrNull() } catch (_: Exception) { null } ?: return@mapNotNull null
            Row(c.title, c.key, "Message request · tap to review", Long.MAX_VALUE, null,
                open = { openChat(c.key, c.device) }, section = Row.Section.Requests)
        }
        if (requests.isNotEmpty()) {
            rows.add(Row("", "", "", 0, null, {}, isSectionHeader = true, sectionTitle = "Message Requests", section = Row.Section.Requests))
            rows.addAll(requests)
        }

        // Section: Group Invitations
        val invites = n.groupInvites().map { i ->
            Row(i.name, i.group, "${Threnody.nameOf(n, i.from)} invites you", Long.MAX_VALUE, null,
                open = { openGroup(i.group) }, section = Row.Section.Invites)
        }
        if (invites.isNotEmpty()) {
            rows.add(Row("", "", "", 0, null, {}, isSectionHeader = true, sectionTitle = "Invitations", section = Row.Section.Invites))
            rows.addAll(invites)
        }

        // Section: Contacts
        val contacts = all.filter { it.accepted }.map { c ->
            val last = try { n.history(c.device, 1u).lastOrNull() } catch (_: Exception) { null }
            Row(c.title, c.key, last?.let { (if (it.outgoing) "You: " else "") + preview(it) } ?: status(c),
                last?.atMs?.toLong() ?: 0, c.connected,
                open = { openChat(c.key, c.device) },
                manage = { manage(n, c.title, c.device, null) },
                section = Row.Section.Contacts)
        }
        if (contacts.isNotEmpty()) {
            rows.add(Row("", "", "", 0, null, {}, isSectionHeader = true, sectionTitle = "Conversations", section = Row.Section.Contacts))
            rows.addAll(contacts)
        }

        // Section: Groups
        val groups = n.groups().map { g ->
            val last = try { n.groupHistory(g.id, 1u).lastOrNull() } catch (_: Exception) { null }
            val who = last?.let { if (it.outgoing) "You" else Threnody.nameOf(n, it.device) }
            Row(g.name, g.id, last?.let { "$who: ${preview(it)}" } ?: members(g.members.size),
                last?.atMs?.toLong() ?: 0, null,
                open = { openGroup(g.id) },
                manage = { manage(n, g.name, null, g.id) },
                section = Row.Section.Groups)
        }
        if (groups.isNotEmpty()) {
            rows.add(Row("", "", "", 0, null, {}, isSectionHeader = true, sectionTitle = "Groups", section = Row.Section.Groups))
            rows.addAll(groups)
        }

        // Section: Anonymous Identities
        val anonymous = Threnody.personaIds().flatMap { id ->
            val p = Threnody.personaNode(id) ?: return@flatMap emptyList()
            val tag = "🎭 ${Threnody.personaLabels[id] ?: "anonymous"}"
            Threnody.conversations(p, id).filter { !it.blocked }.map { c ->
                val last = try { p.history(c.device, 1u).lastOrNull() } catch (_: Exception) { null }
                val what = when {
                    !c.accepted -> "Message request · tap to review"
                    last != null -> (if (last.outgoing) "You: " else "") + preview(last)
                    else -> status(c)
                }
                Row(c.title, c.key, "$tag · $what",
                    if (c.accepted) last?.atMs?.toLong() ?: 0 else Long.MAX_VALUE,
                    c.connected,
                    open = { openChat(c.key, c.device, id) },
                    manage = { manage(p, c.title, c.device, null) },
                    section = Row.Section.Anonymous)
            }
        }
        if (anonymous.isNotEmpty()) {
            rows.add(Row("", "", "", 0, null, {}, isSectionHeader = true, sectionTitle = "Anonymous Identities", section = Row.Section.Anonymous))
            rows.addAll(anonymous)
        }

        // Sort within sections by time (section headers stay at top of their section)
        val sortedRows = rows.groupBy { it.section }.flatMap { (section, sectionRows) ->
            val header = sectionRows.first { it.isSectionHeader }
            val items = sectionRows.filter { !it.isSectionHeader }.sortedByDescending { it.atMs }
            listOf(header) + items
        }

        // Search results stay until the search is cleared.
        runOnUiThread {
            // A newer refresh will draw instead; search results stay put.
            if (gen == refreshGen.get() && (!search.isOpen || search.text.isEmpty())) show(sortedRows)
        }
    }

    /** Searches every conversation, ours and the anonymous identities'. */
    private fun find(q: String) {
        val terms = Search.terms(q)
        val n = ++searches
        if (terms.isEmpty()) return refreshSoon()
        val main = node ?: return
        Threading.background {
            val rows = found(main, q, terms)
            runOnUiThread { if (n == searches && search.isOpen) showFound(rows) }
        }
    }

    /**
     * What a search found (on the worker thread): conversations named for
     * it, then messages, newest first, each tagged with its conversation.
     */
    private fun found(main: ThrenodyNode, q: String, terms: List<String>): Pair<List<Row>, List<Row>> {
        val nodes = listOf<Pair<String?, ThrenodyNode>>(null to main) +
            Threnody.personaIds().mapNotNull { id -> Threnody.personaNode(id)?.let { id to it } }
        val named = mutableListOf<Row>()
        val messages = mutableListOf<Row>()
        val tint = color(R.color.accent)
        for ((persona, n) in nodes) {
            val tag = persona?.let { "🎭 ${Threnody.personaLabels[it] ?: "anonymous"}" }
            val section = if (persona != null) Row.Section.Anonymous else Row.Section.Contacts
            val convos = Threnody.conversations(n, persona)
            val groups = try { n.groups() } catch (_: Exception) { emptyList() }
            for (c in convos) if (!c.blocked && Search.all(c.title, terms)) {
                named.add(Row(c.title, c.key, listOfNotNull(tag, status(c)).joinToString(" · "), 0, c.connected,
                    open = { openChat(c.key, c.device, persona) }, section = section))
            }
            for (g in groups) if (Search.all(g.name, terms)) {
                named.add(Row(g.name, g.id, listOfNotNull(tag, members(g.members.size)).joinToString(" · "), 0, null,
                    open = { openGroup(g.id, persona) }, section = section))
            }
            val hits = try { n.searchAll(q, MAX_RESULTS.toUInt()) } catch (e: Exception) {
                Threnody.say("! search: ${e.message}")
                emptyList()
            }
            val names = mutableMapOf<String, String>()
            for (h in hits) {
                val e = h.entry
                val gid = h.group
                val peer = h.peer
                val title: String
                val key: String
                val open: () -> Unit
                if (gid != null) {
                    title = groups.firstOrNull { it.id == gid }?.name ?: "Group"
                    key = gid
                    open = { openGroup(gid, persona, e, q) }
                } else if (peer != null) {
                    val c = convos.firstOrNull { peer in it.devices }
                    if (c?.blocked == true) continue
                    title = c?.title ?: Threnody.short(peer)
                    key = c?.key ?: peer
                    open = { openChat(key, c?.device ?: peer, persona, e, q) }
                } else continue
                val who = if (e.outgoing) "You" else names.getOrPut(e.device) { Threnody.nameOf(n, e.device) }
                // The text, or the file's name when that's what matched.
                val name = e.file?.name
                val what = if (name != null && Search.ranges(e.text, terms).isEmpty()) "📎 $name" else e.text
                val preview = android.text.SpannableStringBuilder()
                    .append(listOfNotNull(tag, "$who: ").joinToString(" · "))
                    .append(Search.snippet(what, terms, tint))
                messages.add(Row(title, key, preview, e.atMs.toLong(), null, open = open, section = section))
            }
        }
        return named to messages.sortedByDescending { it.atMs }.take(MAX_RESULTS)
    }

    private fun showFound(found: Pair<List<Row>, List<Row>>) {
        val (named, messages) = found
        list.removeAllViews()
        list.layoutAnimation = null
        emptyStateContainer.visibility = View.GONE
        list.visibility = View.VISIBLE
        if (named.isNotEmpty()) {
            addSectionHeader("Conversations")
            named.forEach { list.addView(row(it)) }
        }
        if (messages.isNotEmpty()) {
            if (named.isNotEmpty()) addSectionDivider()
            addSectionHeader("Messages")
            messages.forEach { list.addView(row(it)) }
        }
        if (named.isEmpty() && messages.isEmpty()) {
            list.addView(label("No messages match", Design.typeBody, R.color.muted).apply {
                gravity = Gravity.CENTER
                setPadding(0, dp(Design.xxl), 0, 0)
            }, matchWrap)
        }
        scroll.scrollTo(0, 0)
    }

    private fun preview(e: HistoryEntry) = e.file?.let { Threnody.fileLabel(it.name, it.sensitive, e.text, it.clip) } ?: e.text

    private fun status(c: Conversation) = when {
        c.approved && c.verified -> "Approved · verified"
        c.approved -> "Approved"
        else -> "Not approved yet"
    }

    private fun show(rows: List<Row>) {
        list.removeAllViews()

        val hasContent = rows.any { !it.isSectionHeader }
        emptyStateContainer.visibility = if (hasContent) View.GONE else View.VISIBLE
        list.visibility = if (hasContent) View.VISIBLE else View.GONE

        if (!hasContent) return

        // Apply staggered entrance animation
        val animation = AnimationUtils.loadAnimation(this, android.R.anim.fade_in)
        animation.duration = Design.durationFast.toLong()
        val controller = LayoutAnimationController(animation)
        controller.delay = 0.1f
        controller.order = LayoutAnimationController.ORDER_NORMAL
        list.layoutAnimation = controller

        var lastSection: Row.Section? = null
        for (r in rows) {
            // Add divider between sections
            if (r.isSectionHeader) {
                if (lastSection != null) {
                    addSectionDivider()
                }
                addSectionHeader(r.sectionTitle!!)
                lastSection = r.section
            } else {
                list.addView(row(r))
            }
        }

        list.startLayoutAnimation()
    }

    private fun addSectionHeader(title: String) {
        list.addView(TextView(this).apply {
            text = title.uppercase()
            setTextAppearance(Design.styleLabelMedium)
            setTextColor(color(R.color.muted))
            setTypeface(typeface, Design.weightMedium)
            letterSpacing = 0.05f
            setPadding(dp(Design.listItemPaddingH), dp(Design.lg), dp(Design.listItemPaddingH), dp(Design.sm))
        }, matchWrap)
    }

    private fun addSectionDivider() {
        list.addView(View(this).apply {
            layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, dp(1)).apply {
                topMargin = dp(Design.md)
                bottomMargin = dp(Design.md)
            }
            background = color(R.color.divider).let { ColorDrawable(it) }
        }, matchWrap)
    }

    private fun row(r: Row): View {
        val avatar = Avatar(this, 48).apply { show(r.title, r.avatarKey) }

        val title = label(r.title, Design.typeTitle, R.color.text, Design.weightMedium).apply {
            isSingleLine = true
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
        }

        val previewColor = if (r.atMs == Long.MAX_VALUE) R.color.accent else R.color.muted
        val preview = label("", Design.typeBodySmall, previewColor, Design.weightRegular).apply {
            text = r.preview
            isSingleLine = true
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
        }

        val texts = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(title)
            addView(preview, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(Design.xs) })
        }

        val timeText = if (r.atMs in 1 until Long.MAX_VALUE) ago(r.atMs) else ""
        val time = label(timeText, Design.typeCaption, R.color.muted, Design.weightRegular).apply {
            isSingleLine = true
        }

        val side = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.END
            addView(time, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            if (r.connected != null) {
                val dot = View(this@MainActivity).apply {
                    background = roundedRes(if (r.connected!!) R.color.online else R.color.muted, dp(5).toFloat())
                    contentDescription = if (r.connected!!) "connected" else "not connected"
                    setMinimumWidth(dp(8))
                    setMinimumHeight(dp(8))
                }
                addView(dot, LinearLayout.LayoutParams(dp(10), dp(10)).apply { topMargin = dp(Design.xs); gravity = Gravity.END })
            }
        }

        val rowView = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(Design.listItemPaddingH), dp(Design.listItemPaddingV), dp(Design.listItemPaddingH), dp(Design.listItemPaddingV))
            minimumHeight = dp(76)
            background = ripple()
            addView(avatar)
            addView(texts, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = dp(Design.md); marginEnd = dp(Design.sm) })
            addView(side)
            setOnClickListener { v -> Design.lightHaptic(v); r.open() }
            r.manage?.let { m -> setOnLongClickListener { v -> Design.mediumHaptic(v); m(); true } }
        }

        // Add section color accent for anonymous identities
        if (r.section == Row.Section.Anonymous) {
            val accent = View(this).apply {
                layoutParams = LinearLayout.LayoutParams(dp(3), MATCH_PARENT)
                background = color(R.color.accent).let { ColorDrawable(it) }
            }
            (rowView as LinearLayout).addView(accent, 0)
        }

        return rowView
    }

    private fun ago(ms: Long): String = when {
        System.currentTimeMillis() - ms < DateUtils.MINUTE_IN_MILLIS -> "now"
        DateUtils.isToday(ms) -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_TIME)
        else -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH)
    }

    private fun empty() {
        emptyStateContainer.removeAllViews()
        emptyStateContainer.addView(emptyState(
            title = "No conversations yet",
            message = "Show your invite to someone nearby, or paste theirs. " +
                "Scanning a Threnody QR code with your camera opens it here.",
            actionText = "Show my invite",
            action = { showInvite() },
            iconRes = R.drawable.ic_qr,
        ))
        emptyStateContainer.addView(secondaryButton("Add a contact") { addContact(null) },
            matchWrap.apply { topMargin = dp(Design.md) })
    }

    /** Long-press on a conversation: clear it, or delete the contact. */
    private fun manage(n: ThrenodyNode, title: String, device: String?, group: String?) {
        val options = if (group != null) listOf("Clear chat") else listOf("Clear chat", "Delete contact")
        SecureBuilder(this)
            .setTitle(title)
            .setItems(options.toTypedArray()) { _, i ->
                when (options[i]) {
                    "Clear chat" -> Chats.clear(this, n, title, device, group) { refreshSoon() }
                    else -> Chats.delete(this, n, title, device!!) { refreshSoon() }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun add(anchor: View) {
        PopupMenu(this, anchor).apply {
            menu.add("Scan a QR code").setOnMenuItemClickListener { scan(); true }
            menu.add("Add a contact").setOnMenuItemClickListener { addContact(null); true }
            menu.add("New group").setOnMenuItemClickListener { newGroup(); true }
            menu.add("Anonymous invite").setOnMenuItemClickListener { newPersona(); true }
            show()
        }
    }

    private fun newGroup() {
        val field = input("Group name", null, InputType.TYPE_TEXT_FLAG_CAP_WORDS)
        SecureBuilder(this)
            .setTitle("New group")
            .setMessage("You'll own the group: only you can add and remove members. Everyone in it sees who else is.")
            .setView(padded(field))
            .setPositiveButton("Create") { _, _ ->
                val name = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                if (name.isEmpty()) return@setPositiveButton
                Threading.background {
                    try {
                        val id = n.createGroup(name)
                        runOnUiThread { openGroup(id) }
                    } catch (e: Exception) {
                        runOnUiThread { failed("Couldn't create the group: ${e.message}") }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun openGroup(id: String, persona: String? = null, jump: HistoryEntry? = null, query: String? = null) {
        startActivity(Intent(this, ChatActivity::class.java)
            .putExtra(ChatActivity.GROUP, id)
            .putExtra(ChatActivity.PERSONA, persona)
            .jumpTo(jump, query))
    }

    /** Opens the chat at a search result, with the search still open on it. */
    private fun Intent.jumpTo(e: HistoryEntry?, query: String?): Intent {
        if (e == null) return this
        return putExtra(ChatActivity.JUMP_AT_MS, e.atMs.toLong())
            .putExtra(ChatActivity.JUMP_DEVICE, e.device)
            .putExtra(ChatActivity.JUMP_QUERY, query)
    }

    private fun more(anchor: View) {
        val popup = PopupMenu(this, anchor)
        val menu = popup.menu

        // Account & Identity
        val accountSection = menu.addSubMenu("Account & Identity")
        accountSection.add("Devices").setOnMenuItemClickListener { devices(); true }
        accountSection.add("Your profile").setOnMenuItemClickListener {
            node?.let { ProfileUi.edit(this@MainActivity, it, "Your profile") }
            true
        }
        accountSection.add("Anonymous identities").setOnMenuItemClickListener { personas(); true }
        accountSection.add("Credentials").setOnMenuItemClickListener {
            node?.let { CredentialUi.list(this@MainActivity, it) }
            true
        }
        accountSection.add("Link a new device").setOnMenuItemClickListener { linkDevice(); true }
        accountSection.add("Join another device's account").setOnMenuItemClickListener { joinAccount(null); true }

        // Privacy & Security
        val privacySection = menu.addSubMenu("Privacy & Security")
        fun toggle(title: String, on: Boolean, set: (Boolean) -> Unit, says: (Boolean) -> String) =
            privacySection.add(title).apply {
                isCheckable = true
                isChecked = on
                setOnMenuItemClickListener {
                    set(!on)
                    Toast.makeText(this@MainActivity, says(!on), Toast.LENGTH_LONG).show()
                    true
                }
            }
        toggle("Cover traffic", Privacy.coverTraffic(this@MainActivity),
            { Privacy.setCoverTraffic(this@MainActivity, it) }) {
            if (it) "Cover traffic on: traffic no longer shows when you send"
            else "Cover traffic off: an observer can see when messages are sent"
        }
        toggle("Onion routing first", Privacy.onionFirst(this@MainActivity),
            { Privacy.setOnionFirst(this@MainActivity, it) }) {
            if (it) "Contacts are reached through onion circuits when possible"
            else "Contacts are dialed directly: the network sees who you talk to"
        }
        toggle("Remove photo metadata", Privacy.stripMetadata(this@MainActivity),
            { Privacy.setStripMetadata(this@MainActivity, it) }) {
            if (it) "Photos are sent without location, camera or time details"
            else "Photos are sent with their metadata, which can include where they were taken"
        }
        toggle("Screen security", Privacy.screenSecurity(this@MainActivity),
            { Privacy.setScreenSecurity(this@MainActivity, it) }) {
            if (it) "Screenshots blocked; hidden in recent apps" else "Screenshots allowed"
        }

        // Network & Connectivity
        val networkSection = menu.addSubMenu("Network & Connectivity")
        toggle("Reach contacts over the internet", Privacy.reachInternet(this@MainActivity),
            { Privacy.setReachInternet(this@MainActivity, it) }) {
            if (it) "Contacts can be reached on mobile data and other networks. " +
                "Strangers in the public DHT see this phone's IP address, not who you talk to"
            else "Contacts are reached only nearby, through relays, or at addresses you dial"
        }
        toggle("Route through volunteer relays", Privacy.useVolunteers(this@MainActivity),
            { Privacy.setUseVolunteers(this@MainActivity, it) }) {
            if (it) "When contacts can't relay for you, circuits go through volunteers from your directories"
            else "Only your own contacts relay for you"
        }
        networkSection.add("Relay directories").setOnMenuItemClickListener { DirectoryUi.show(this@MainActivity); true }

        // Messaging
        val messagingSection = menu.addSubMenu("Messaging")
        toggle("Send read receipts", Privacy.sendReadReceipts(this@MainActivity),
            { Privacy.setSendReadReceipts(this@MainActivity, it) }) {
            if (it) "When you open a chat, the sender learns you've displayed their messages"
            else "The sender won't know when you've read their messages"
        }
        toggle("Send typing indicators", Privacy.sendTyping(this@MainActivity),
            { Privacy.setSendTyping(this@MainActivity, it) }) {
            if (it) "Contacts see “…” while you write to them"
            else "Contacts don't see when you're typing"
        }
        toggle("GIF search with GIPHY", Privacy.giphy(this@MainActivity),
            { Privacy.setGiphy(this@MainActivity, it) }) {
            if (it) "GIF search on: GIPHY sees your searches and this phone's IP address"
            else "GIF search off: the GIF button offers only GIFs on this phone"
        }
        messagingSection.add("Default disappearing timer").setOnMenuItemClickListener { defaultTimer(); true }

        // Advanced
        val advancedSection = menu.addSubMenu("Advanced")
        advancedSection.add("Diagnostics").setOnMenuItemClickListener {
            startActivity(Intent(this@MainActivity, LogActivity::class.java)); true
        }

        popup.show()
    }

    private fun openChat(key: String, device: String, persona: String? = null, jump: HistoryEntry? = null, query: String? = null) {
        startActivity(Intent(this, ChatActivity::class.java)
            .putExtra(ChatActivity.KEY, key)
            .putExtra(ChatActivity.DEVICE, device)
            .putExtra(ChatActivity.PERSONA, persona)
            .jumpTo(jump, query))
    }

    /** Burn-after choices for a new anonymous identity (null = keep it). */
    private val burnChoices = listOf<Pair<String, Long?>>(
        "Keep it until I burn it" to null,
        "Burn after 1 day" to DateUtils.DAY_IN_MILLIS,
        "Burn after 1 week" to DateUtils.WEEK_IN_MILLIS,
        "Burn after 4 weeks" to 4 * DateUtils.WEEK_IN_MILLIS,
    )

    /** A new anonymous identity and its invite. */
    private fun newPersona() {
        val field = input("Your label for it (only you see this)", null, InputType.TYPE_TEXT_FLAG_CAP_SENTENCES)
        var burn = 2
        val box = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), 0)
            addView(field, matchWrap)
            val group = android.widget.RadioGroup(this@MainActivity)
            burnChoices.forEachIndexed { i, (name, _) ->
                group.addView(android.widget.RadioButton(this@MainActivity).apply {
                    id = i + 1
                    text = name
                    isChecked = i == burn
                })
            }
            group.setOnCheckedChangeListener { _, id -> burn = id - 1 }
            addView(group, matchWrap)
        }
        SecureBuilder(this)
            .setTitle("Anonymous invite")
            .setMessage(
                "A new identity with its own keys and conversations. Nothing links it to you unless you reveal it. " +
                    "Anyone you reach directly can still see your network address.",
            )
            .setView(ScrollView(this).apply { addView(box) })
            .setPositiveButton("Create") { _, _ ->
                val label = field.text.toString().trim().ifEmpty { "Anonymous" }
                val expires = burnChoices[burn].second?.let { System.currentTimeMillis() + it }
                Threading.background {
                    try {
                        val rec = Threnody.createPersona(this, label, expires)
                        runOnUiThread { personaInvite(rec.id) }
                        refresh()
                    } catch (e: Exception) {
                        runOnUiThread { failed("Couldn't create it: ${e.message}") }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** An anonymous identity's invite, on its own port. */
    private fun personaInvite(id: String) {
        val p = Threnody.personaNode(id) ?: return
        val ip = ourAddr()?.substringBeforeLast(':') ?: return failed("No network: connect to Wi-Fi to make an invite.")
        val link = p.inviteLink("$ip:${Threnody.personaPort(this, id)}")
        showCode(
            "Anonymous invite · ${Threnody.personaLabels[id] ?: ""}",
            "Whoever uses this reaches your anonymous identity, not you. It works while this phone is on this network.",
            link,
            "Anonymous identity ${p.deviceFingerprint()}",
        )
    }

    /** The anonymous identities, each with its invite, profile, rename and burn. */
    private fun personas() {
        val n = node ?: return
        Threading.background {
            val list = try { n.personas() } catch (_: Exception) { emptyList() }
            runOnUiThread {
                if (list.isEmpty()) {
                    SecureBuilder(this)
                        .setTitle("Anonymous identities")
                        .setMessage("None yet. + → Anonymous invite makes one.")
                        .setPositiveButton("Make one") { _, _ -> newPersona() }
                        .setNegativeButton("Close", null)
                        .show()
                    return@runOnUiThread
                }
                val rows = list.map { p ->
                    p.label + (p.expiresMs?.let { " · burns " + DateUtils.getRelativeTimeSpanString(it.toLong()).toString().replaceFirstChar(Char::lowercase) } ?: "")
                }
                SecureBuilder(this)
                    .setTitle("Anonymous identities")
                    .setItems(rows.toTypedArray()) { _, i -> persona(list[i].id, list[i].label) }
                    .setPositiveButton("New") { _, _ -> newPersona() }
                    .setNegativeButton("Close", null)
                    .show()
            }
        }
    }

    private fun persona(id: String, label: String) {
        val p = Threnody.personaNode(id) ?: return
        val options = listOf("Show its invite", "Its profile", "Rename", "Burn it")
        SecureBuilder(this)
            .setTitle(label)
            .setItems(options.toTypedArray()) { _, i ->
                when (i) {
                    0 -> personaInvite(id)
                    1 -> ProfileUi.edit(this, p, "$label's profile")
                    2 -> renamePersona(id, label)
                    3 -> burnPersona(id, label)
                }
            }
            .setNegativeButton("Close", null)
            .show()
    }

    private fun renamePersona(id: String, label: String) {
        val field = input("Label", label)
        SecureBuilder(this)
            .setTitle("Rename")
            .setView(padded(field))
            .setPositiveButton("Save") { _, _ ->
                val new = field.text.toString().trim()
                if (new.isNotEmpty()) Threading.background {
                    try { Threnody.renamePersona(this, id, new) } catch (e: Exception) { runOnUiThread { failed("${e.message}") } }
                    refresh()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun burnPersona(id: String, label: String) {
        SecureBuilder(this)
            .setTitle("Burn $label?")
            .setMessage("Its keys, contacts, messages and files are deleted for good. Nobody can reach it again.")
            .setPositiveButton("Burn") { _, _ ->
                Threading.background {
                    try { Threnody.burnPersona(this, id) } catch (e: Exception) { runOnUiThread { failed("${e.message}") } }
                    refresh()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** The address others should dial: our Wi-Fi (or other) IPv4 address. */
    private fun ourAddr(): String? {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return null
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return null
        val ip = props.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }?.hostAddress ?: return null
        return "$ip:${Threnody.listenAddr.substringAfterLast(':')}"
    }

    private fun showInvite() {
        val n = node ?: return
        val addr = ourAddr() ?: return failed("No network: connect to Wi-Fi to make an invite.")
        val link = n.inviteLink(addr)
        showCode(
            "Your invite",
            "Let the other person scan this, or send them the link. " +
                "Anyone with it can contact this device.",
            link,
            "Device ${n.deviceFingerprint()}",
        )
    }

    /** The disappearing timer for chats that haven't chosen their own. */
    private fun defaultTimer() {
        val choices = Privacy.TIMERS
        val checked = choices.indexOfFirst { it.second == Privacy.defaultTimer(this) }
        SecureBuilder(this)
            .setTitle("Default disappearing timer")
            .setSingleChoiceItems(choices.map { it.first }.toTypedArray(), checked) { d, i ->
                d.dismiss()
                Privacy.setDefaultTimer(this, choices[i].second)
                Toast.makeText(this, "New chats: ${choices[i].first.lowercase()}", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** This account's devices; tap one to rename it, or to remove another one. */
    private fun devices() {
        val n = node ?: return
        Threading.background {
            val list = n.devices()
            runOnUiThread {
                val labels = list.map { d ->
                    d.name + (if (d.thisDevice) " (this device)" else "") + "\n" + Threnody.short(d.fingerprint)
                }
                SecureBuilder(this)
                    .setTitle("Your devices")
                    .setItems(labels.toTypedArray()) { _, i -> deviceActions(list[i], list.size) }
                    .setPositiveButton("Link a new device") { _, _ -> linkDevice() }
                    .setNegativeButton("Close", null)
                    .show()
            }
        }
    }

    /** Rename; for another device (not the account's only one), remove it too. */
    private fun deviceActions(d: DeviceInfo, count: Int) {
        if (d.thisDevice || count < 2) return renameDevice(d.fingerprint, d.name)
        SecureBuilder(this)
            .setTitle(d.name)
            .setItems(arrayOf("Rename", "Remove from account")) { _, i ->
                if (i == 0) renameDevice(d.fingerprint, d.name) else removeDevice(d)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun removeDevice(d: DeviceInfo) {
        SecureBuilder(this)
            .setTitle("Remove ${d.name}?")
            .setMessage("It leaves your account: your other devices and your contacts stop trusting it, " +
                "and it gets no more of your messages. Do this for a lost or retired device. " +
                "To use it again, link it anew.")
            .setPositiveButton("Remove") { _, _ ->
                val n = node ?: return@setPositiveButton
                Threading.background {
                    try {
                        n.removeDevice(d.fingerprint)
                        runOnUiThread { Toast.makeText(this, "Removed ${d.name}", Toast.LENGTH_SHORT).show() }
                    } catch (e: Exception) {
                        runOnUiThread { failed("Couldn't remove it: ${e.message}") }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun renameDevice(fingerprint: String, current: String) {
        val field = input("Name", current, InputType.TYPE_TEXT_FLAG_CAP_WORDS)
        SecureBuilder(this)
            .setTitle("Rename device")
            .setMessage("Your other devices and your contacts see this name.")
            .setView(padded(field))
            .setPositiveButton("Save") { _, _ ->
                val name = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                if (name.isEmpty()) return@setPositiveButton
                Threading.background {
                    try {
                        n.renameDevice(fingerprint, name)
                        runOnUiThread { Toast.makeText(this, "Renamed to $name", Toast.LENGTH_SHORT).show() }
                    } catch (e: Exception) {
                        runOnUiThread { failed("Couldn't rename: ${e.message}") }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun linkDevice() {
        val n = node ?: return
        val addr = ourAddr() ?: return failed("No network: both devices need to be on the same network.")
        showCode(
            "Link a new device",
            "On the new device, choose “Join another device's account” and scan or paste this. " +
                "It works once. Only show it to your own devices.",
            n.createLinkCode(addr),
            "Account ${n.accountFingerprint()}",
        )
    }

    /** A dialog showing [code] as a QR code, with copy and share. */
    private fun showCode(title: String, help: String, code: String, footer: String) {
        val box = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(dp(Design.xl), dp(Design.md), dp(Design.xl), dp(Design.md))
        }
        box.addView(label(help, Design.typeBody, R.color.text_secondary, maxLines = 0, weight = Design.weightMedium).apply {
            gravity = Gravity.CENTER_HORIZONTAL
        }, matchWrap)
        try {
            box.addView(QrView(this, qrMatrix(code)), LinearLayout.LayoutParams(dp(240), dp(240)).apply {
                topMargin = dp(Design.lg); bottomMargin = dp(Design.lg)
                gravity = Gravity.CENTER_HORIZONTAL
            })
        } catch (e: Exception) {
            Threnody.say("! QR generation failed: ${e.message}")
        }
        box.addView(label(code, Design.typeBody, R.color.md_sys_color_on_surface, weight = Design.weightBold, maxLines = 0), matchWrap)
        box.addView(label(footer, Design.typeCaption, R.color.text_secondary, maxLines = 0).apply {
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(0, dp(Design.md), 0, 0)
        }, matchWrap)
        SecureBuilder(this)
            .setTitle(title)
            .setView(android.widget.ScrollView(this).apply { addView(box) })
            .setPositiveButton("Share") { _, _ ->
                startActivity(Intent.createChooser(
                    Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, code), title))
            }
            .setNeutralButton("Copy") { _, _ ->
                getSystemService(ClipboardManager::class.java)?.setPrimaryClip(ClipData.newPlainText(title, code))
                Toast.makeText(this, "Copied", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("Done", null)
            .show()
    }

    /** A one-value field (a name, a link) that wraps when long. */
    private fun input(hint: String, value: String?, flags: Int = InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS) = EditText(this).apply {
        this.hint = hint
        inputType = InputType.TYPE_CLASS_TEXT or flags
        wrapping(newlines = false, max = 4)
        setText(value ?: "")
    }

    private fun padded(v: View) = LinearLayout(this).apply {
        setPadding(dp(24), dp(8), dp(24), 0)
        addView(v, matchWrap)
    }

    /** `scanned`: the invite came from a QR code, so naming them comes next. */
    private fun addContact(prefill: String?, scanned: Boolean = false) {
        // A copied invite (say, from the camera app) fills itself in.
        val field = input("threnody://… or host:port", prefill ?: copiedLink()?.takeIf { it.startsWith("threnody://") })
        val name = input("Name (optional; only you see it)", null,
            InputType.TYPE_TEXT_FLAG_CAP_WORDS or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
        val fields = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), 0)
            addView(field, matchWrap)
            addView(name, matchWrap)
        }
        val dialog = SecureBuilder(this)
            .setTitle(if (scanned) "Add the scanned contact" else "Add a contact")
            .setMessage(
                if (scanned) "Give them a name you'll recognise, if you like. You'll check their safety number together later."
                else "Scan or paste their invite. You'll check their safety number together later."
            )
            .setView(fields)
            .setPositiveButton("Connect") { _, _ -> connect(field.text.toString().trim(), name.text.toString().trim()) }
            .setNeutralButton("Scan") { _, _ -> scan() }
            .setNegativeButton("Cancel", null)
            .show()
        if (scanned) {
            name.requestFocus()
            dialog.window?.setSoftInputMode(android.view.WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
        }
    }

    private val scanLauncher = ActivityResultRegistry.get(this)

    private fun scan() {
        scanLauncher.launch(Intent(this, ScanActivity::class.java)) { resultCode, data ->
            if (resultCode != RESULT_OK) return@launch
            val text = data?.getStringExtra(ScanActivity.RESULT)?.trim() ?: return@launch
            when {
                text.startsWith("threnody://") -> addContact(text, scanned = true)
                text.startsWith("threnody-link://") -> joinAccount(text)
                text.startsWith(DirectoryUi.SCHEME) -> DirectoryUi.add(this, text)
                else -> failed("That QR code isn't a Threnody invite, link code or directory link.")
            }
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (!ActivityResultRegistry.dispatch(this, requestCode, resultCode, data)) {
            super.onActivityResult(requestCode, resultCode, data)
        }
    }

    /** A Threnody invite or link code on the clipboard, if any. */
    private fun copiedLink(): String? = try {
        getSystemService(ClipboardManager::class.java)?.primaryClip
            ?.takeIf { it.itemCount > 0 }
            ?.getItemAt(0)?.coerceToText(this)?.toString()?.trim()
            ?.takeIf { it.startsWith("threnody://") || it.startsWith("threnody-link://") || it.startsWith(DirectoryUi.SCHEME) }
    } catch (_: SecurityException) {
        null
    }

    /** The copied link last offered, so it's offered once. */
    private var offered: String? = null

    // The clipboard can only be read while the window has focus.
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus) return
        val link = copiedLink() ?: return
        val n = node ?: return
        if (link == offered) return
        // Not our own invite or link code (say, just copied from "My invite").
        if (link.contains(n.deviceFingerprint().replace("-", ""))) return
        offered = link
        if (link.startsWith(DirectoryUi.SCHEME)) {
            SecureBuilder(this)
                .setTitle("Subscribe to the copied directory?")
                .setMessage(link)
                .setPositiveButton("Subscribe") { _, _ -> DirectoryUi.subscribe(this, link) }
                .setNegativeButton("Not now", null)
                .show()
            return
        }
        val invite = link.startsWith("threnody://")
        SecureBuilder(this)
            .setTitle(if (invite) "Add the copied invite?" else "Join with the copied link code?")
            .setMessage(link)
            .setPositiveButton(if (invite) "Connect" else "Continue") { _, _ ->
                if (invite) connect(link) else joinAccount(link)
            }
            .setNegativeButton("Not now", null)
            .show()
    }

    /** Dials `target`; a `name` given is what we call them (only here). */
    private fun connect(target: String, name: String = "") {
        if (target.isEmpty()) return
        val n = node ?: return
        Toast.makeText(this, "Connecting…", Toast.LENGTH_SHORT).show()
        Threading.background {
            try {
                val peer = n.connect(target)
                if (name.isNotEmpty()) {
                    try { n.setName(peer, name) } catch (e: Exception) { Threnody.say("! naming a contact: ${e.message}") }
                }
                val key = Threnody.key(n.contacts(), peer)
                runOnUiThread { openChat(key, peer) }
            } catch (e: Exception) {
                runOnUiThread { failed("Couldn't connect: ${e.message}") }
            }
        }
    }

    private fun joinAccount(prefill: String?) {
        val field = input("threnody-link://…", prefill)
        val prefilled = prefill ?: copiedLink()?.takeIf { it.startsWith("threnody-link://") }
        field.setText(prefilled ?: "")
        SecureBuilder(this)
            .setTitle("Join another device's account")
            .setMessage("This device becomes part of that account: your contacts see both as you. " +
                "Get the code from “Link a new device” on the other device.")
            .setView(padded(field))
            .setPositiveButton("Join") { _, _ ->
                val code = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                Threading.background {
                    try {
                        val account = n.linkWith(code)
                        runOnUiThread { Toast.makeText(this, "Joined account ${Threnody.short(account)}", Toast.LENGTH_LONG).show() }
                        refresh()
                    } catch (e: Exception) {
                        runOnUiThread { failed("Couldn't join: ${e.message}") }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** `threnody://` invites and `threnody-link://` codes, from a QR scan or a tapped link. */
    private fun handleLink(intent: Intent?) {
        val uri = intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString ?: return
        // Handle each link once, not again after rotation.
        intent.action = null
        when {
            uri.startsWith("threnody://") -> addContact(uri)
            uri.startsWith("threnody-link://") -> joinAccount(uri)
            uri.startsWith(DirectoryUi.SCHEME) -> DirectoryUi.add(this, uri)
        }
    }

    private fun failed(msg: String) {
        SecureBuilder(this).setMessage(msg).setPositiveButton("OK", null).show()
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
        /** Most messages a search lists. */
        private const val MAX_RESULTS = 200
    }
}