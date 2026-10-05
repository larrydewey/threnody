package org.threnody.app

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
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
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.net.Inet4Address
import java.util.concurrent.Executors
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.ThrenodyNode
import uniffi.threnody_ffi.qrMatrix

/** The conversation list, plus invites, adding contacts and device linking. */
class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private var node: ThrenodyNode? = null
    private lateinit var list: LinearLayout
    private lateinit var scroll: ScrollView
    private var unsubscribe: (() -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val bar = TopBar(this, null).apply {
            title.text = "Threnody"
            action(R.drawable.ic_qr, "My invite") { showInvite() }
            action(R.drawable.ic_add, "New conversation") { add(it) }
            action(R.drawable.ic_more, "More") { more(it) }
        }
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        scroll = ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            addView(list, MATCH_PARENT, WRAP_CONTENT)
        }
        root.addView(bar, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, scroll)

        ThrenodyService.start(this)
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 3)
        }
        worker.execute {
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
        unsubscribe = Threnody.subscribe { worker.execute { refresh() } }
        worker.execute { refresh() }
    }

    override fun onStop() {
        Threnody.visible--
        unsubscribe?.invoke()
        super.onStop()
    }

    /** One line in the list: a contact, a group, or an invitation to one. */
    private data class Row(
        val title: String,
        val avatarKey: String,
        val preview: String,
        val atMs: Long,
        /** Null for groups, which have no single connection. */
        val connected: Boolean?,
        val open: () -> Unit,
        /** Long-press: clear or delete it. */
        val manage: (() -> Unit)? = null,
    )

    /** Reloads the list (on the worker thread). */
    private fun refresh() {
        val n = node ?: return
        val all = Threnody.conversations(n).filter { !it.blocked }
        // Someone we haven't accepted, who wrote to us: a request.
        val requests = all.filter { !it.accepted }.mapNotNull { c ->
            val last = try { n.history(c.device, 1u).lastOrNull() } catch (_: Exception) { null } ?: return@mapNotNull null
            Row(c.title, c.key, "Message request · tap to review", Long.MAX_VALUE, null, open = { openChat(c.key, c.device) })
        }
        val contacts = all.filter { it.accepted }.map { c ->
            val last = try { n.history(c.device, 1u).lastOrNull() } catch (_: Exception) { null }
            Row(c.title, c.key, last?.let { (if (it.outgoing) "You: " else "") + preview(it) } ?: status(c),
                last?.atMs?.toLong() ?: 0, c.connected, { openChat(c.key, c.device) }) { manage(n, c.title, c.device, null) }
        }
        val groups = n.groups().map { g ->
            val last = try { n.groupHistory(g.id, 1u).lastOrNull() } catch (_: Exception) { null }
            val who = last?.let { if (it.outgoing) "You" else Threnody.nameOf(n, it.device) }
            Row(g.name, g.id, last?.let { "$who: ${preview(it)}" } ?: members(g.members.size),
                last?.atMs?.toLong() ?: 0, null, { openGroup(g.id) }) { manage(n, g.name, null, g.id) }
        }
        // Invitations go first: they wait on the user.
        val invites = n.groupInvites().map { i ->
            Row(i.name, i.group, "${Threnody.nameOf(n, i.from)} invites you", Long.MAX_VALUE, null, open = { openGroup(i.group) })
        }
        // Anonymous identities' conversations, marked with the identity's label.
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
                Row(c.title, c.key, "$tag · $what", if (c.accepted) last?.atMs?.toLong() ?: 0 else Long.MAX_VALUE,
                    c.connected, { openChat(c.key, c.device, id) }) { manage(p, c.title, c.device, null) }
            }
        }
        val rows = requests + invites + (contacts + groups + anonymous).sortedByDescending { it.atMs }
        runOnUiThread { show(rows) }
    }

    private fun preview(e: HistoryEntry) = e.file?.let { Threnody.fileLabel(it.name, it.sensitive, e.text) } ?: e.text

    private fun show(rows: List<Row>) {
        list.removeAllViews()
        if (rows.isEmpty()) return empty()
        for (r in rows) list.addView(row(r))
    }

    private fun row(r: Row): View {
        val avatar = Avatar(this, 44).apply { show(r.title, r.avatarKey) }
        val title = label(r.title, 16f).apply { isSingleLine = true }
        val preview = label(r.preview, 14f, if (r.atMs == Long.MAX_VALUE) R.color.accent else R.color.muted)
            .apply { isSingleLine = true }
        val texts = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(title)
            addView(preview)
        }
        val time = label(if (r.atMs in 1 until Long.MAX_VALUE) ago(r.atMs) else "", 12f, R.color.muted)
            .apply { isSingleLine = true }
        val side = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.END
            addView(time, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            if (r.connected != null) {
                val dot = View(this@MainActivity).apply {
                    background = rounded(color(if (r.connected) R.color.online else R.color.divider), dp(5).toFloat())
                    contentDescription = if (r.connected) "connected" else "not connected"
                }
                addView(dot, LinearLayout.LayoutParams(dp(10), dp(10)).apply { topMargin = dp(8); gravity = Gravity.END })
            }
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(16), dp(12), dp(16), dp(12))
            minimumHeight = dp(72)
            background = getDrawable(android.R.drawable.list_selector_background)
            addView(avatar)
            addView(texts, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = dp(14); marginEnd = dp(8) })
            addView(side)
            setOnClickListener { r.open() }
            r.manage?.let { m -> setOnLongClickListener { m(); true } }
        }
    }

    /** Long-press on a conversation: clear it, or delete the contact. */
    private fun manage(n: ThrenodyNode, title: String, device: String?, group: String?) {
        val options = if (group != null) listOf("Clear chat") else listOf("Clear chat", "Delete contact")
        AlertDialog.Builder(this)
            .setTitle(title)
            .setItems(options.toTypedArray()) { _, i ->
                when (options[i]) {
                    "Clear chat" -> Chats.clear(this, n, worker, title, device, group) { worker.execute { refresh() } }
                    else -> Chats.delete(this, n, worker, title, device!!) { worker.execute { refresh() } }
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
        val field = input("Group name", null).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
        }
        AlertDialog.Builder(this)
            .setTitle("New group")
            .setMessage("You'll own the group: only you can add and remove members. Everyone in it sees who else is.")
            .setView(padded(field))
            .setPositiveButton("Create") { _, _ ->
                val name = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                if (name.isEmpty()) return@setPositiveButton
                worker.execute {
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

    private fun openGroup(id: String) {
        startActivity(Intent(this, ChatActivity::class.java).putExtra(ChatActivity.GROUP, id))
    }

    private fun status(c: Conversation) = when {
        c.approved && c.verified -> "Approved · verified"
        c.approved -> "Approved"
        else -> "Not approved yet"
    }

    private fun ago(ms: Long): String = when {
        System.currentTimeMillis() - ms < DateUtils.MINUTE_IN_MILLIS -> "now"
        DateUtils.isToday(ms) -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_TIME)
        else -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH)
    }

    private fun empty() {
        val box = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(dp(32), dp(48), dp(32), dp(48))
        }
        box.addView(label("No contacts yet", 20f).apply { gravity = Gravity.CENTER }, matchWrap)
        box.addView(label(
            "Show your invite to someone nearby, or paste theirs. " +
                "Scanning a Threnody QR code with your camera opens it here.",
            15f, R.color.muted,
        ).apply { gravity = Gravity.CENTER; setPadding(0, dp(8), 0, dp(24)) }, matchWrap)
        box.addView(primary("Show my invite") { showInvite() }, matchWrap)
        box.addView(secondary("Add a contact") { addContact(null) }, matchWrap.apply { topMargin = dp(8) })
        list.addView(box, LinearLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
    }

    private fun primary(text: String, onClick: () -> Unit) = Button(this).apply {
        this.text = text
        isAllCaps = false
        setTextColor(color(R.color.on_accent))
        background = rounded(color(R.color.accent), dp(24).toFloat())
        minHeight = dp(48)
        setOnClickListener { onClick() }
    }

    private fun secondary(text: String, onClick: () -> Unit) = Button(this).apply {
        this.text = text
        isAllCaps = false
        setTextColor(color(R.color.accent))
        background = getDrawable(android.R.drawable.list_selector_background)
        minHeight = dp(48)
        setOnClickListener { onClick() }
    }

    private fun more(anchor: View) {
        PopupMenu(this, anchor).apply {
            menu.add("Devices").setOnMenuItemClickListener { devices(); true }
            menu.add("Your profile").setOnMenuItemClickListener {
                node?.let { ProfileUi.edit(this@MainActivity, it, "Your profile", worker) }
                true
            }
            menu.add("Anonymous identities").setOnMenuItemClickListener { personas(); true }
            fun toggle(title: String, on: Boolean, set: (Boolean) -> Unit, says: (Boolean) -> String) =
                menu.add(title).apply {
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
            menu.add("Default disappearing timer").setOnMenuItemClickListener { defaultTimer(); true }
            menu.add("Screen security").apply {
                isCheckable = true
                isChecked = Privacy.screenSecurity(this@MainActivity)
                setOnMenuItemClickListener {
                    val on = !Privacy.screenSecurity(this@MainActivity)
                    Privacy.setScreenSecurity(this@MainActivity, on)
                    Toast.makeText(
                        this@MainActivity,
                        if (on) "Screenshots blocked; hidden in recent apps" else "Screenshots allowed",
                        Toast.LENGTH_SHORT,
                    ).show()
                    true
                }
            }
            menu.add("Link a new device").setOnMenuItemClickListener { linkDevice(); true }
            menu.add("Join another device's account").setOnMenuItemClickListener { joinAccount(null); true }
            menu.add("Diagnostics").setOnMenuItemClickListener {
                startActivity(Intent(this@MainActivity, LogActivity::class.java)); true
            }
            show()
        }
    }

    private fun openChat(key: String, device: String, persona: String? = null) {
        startActivity(Intent(this, ChatActivity::class.java)
            .putExtra(ChatActivity.KEY, key)
            .putExtra(ChatActivity.DEVICE, device)
            .putExtra(ChatActivity.PERSONA, persona))
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
        val field = input("Your label for it (only you see this)", null).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
        }
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
        AlertDialog.Builder(this)
            .setTitle("Anonymous invite")
            .setMessage(
                "A new identity with its own keys and conversations. Nothing links it to you unless you reveal it. " +
                    "Anyone you reach directly can still see your network address.",
            )
            .setView(ScrollView(this).apply { addView(box) })
            .setPositiveButton("Create") { _, _ ->
                val label = field.text.toString().trim().ifEmpty { "Anonymous" }
                val expires = burnChoices[burn].second?.let { System.currentTimeMillis() + it }
                worker.execute {
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
        worker.execute {
            val list = try { n.personas() } catch (_: Exception) { emptyList() }
            runOnUiThread {
                if (list.isEmpty()) {
                    AlertDialog.Builder(this)
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
                AlertDialog.Builder(this)
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
        AlertDialog.Builder(this)
            .setTitle(label)
            .setItems(options.toTypedArray()) { _, i ->
                when (i) {
                    0 -> personaInvite(id)
                    1 -> ProfileUi.edit(this, p, "$label's profile", worker)
                    2 -> renamePersona(id, label)
                    3 -> burnPersona(id, label)
                }
            }
            .setNegativeButton("Close", null)
            .show()
    }

    private fun renamePersona(id: String, label: String) {
        val field = input("Label", label)
        AlertDialog.Builder(this)
            .setTitle("Rename")
            .setView(padded(field))
            .setPositiveButton("Save") { _, _ ->
                val new = field.text.toString().trim()
                if (new.isNotEmpty()) worker.execute {
                    try { Threnody.renamePersona(this, id, new) } catch (e: Exception) { runOnUiThread { failed("${e.message}") } }
                    refresh()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun burnPersona(id: String, label: String) {
        AlertDialog.Builder(this)
            .setTitle("Burn $label?")
            .setMessage("Its keys, contacts, messages and files are deleted for good. Nobody can reach it again.")
            .setPositiveButton("Burn") { _, _ ->
                worker.execute {
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
        AlertDialog.Builder(this)
            .setTitle("Default disappearing timer")
            .setSingleChoiceItems(choices.map { it.first }.toTypedArray(), checked) { d, i ->
                d.dismiss()
                Privacy.setDefaultTimer(this, choices[i].second)
                Toast.makeText(this, "New chats: ${choices[i].first.lowercase()}", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** This account's devices; tap one to rename it. */
    private fun devices() {
        val n = node ?: return
        worker.execute {
            val list = n.devices()
            runOnUiThread {
                val labels = list.map { d ->
                    d.name + (if (d.thisDevice) " (this device)" else "") + "\n" + Threnody.short(d.fingerprint)
                }
                AlertDialog.Builder(this)
                    .setTitle("Your devices")
                    .setItems(labels.toTypedArray()) { _, i -> renameDevice(list[i].fingerprint, list[i].name) }
                    .setPositiveButton("Link a new device") { _, _ -> linkDevice() }
                    .setNegativeButton("Close", null)
                    .show()
            }
        }
    }

    private fun renameDevice(fingerprint: String, current: String) {
        val field = input("Name", current).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
        }
        AlertDialog.Builder(this)
            .setTitle("Rename device")
            .setMessage("Your other devices and your contacts see this name.")
            .setView(padded(field))
            .setPositiveButton("Save") { _, _ ->
                val name = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                if (name.isEmpty()) return@setPositiveButton
                worker.execute {
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
            setPadding(dp(24), dp(8), dp(24), 0)
        }
        box.addView(label(help, 14f, R.color.muted), matchWrap)
        try {
            box.addView(QrView(this, qrMatrix(code)), LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply {
                topMargin = dp(16); bottomMargin = dp(16)
            })
        } catch (_: Exception) {}
        box.addView(label(code, 12f, R.color.muted).apply { setTextIsSelectable(true); typeface = android.graphics.Typeface.MONOSPACE }, matchWrap)
        box.addView(label(footer, 12f, R.color.muted).apply { setTextIsSelectable(true); setPadding(0, dp(8), 0, 0) }, matchWrap)
        AlertDialog.Builder(this)
            .setTitle(title)
            .setView(ScrollView(this).apply { addView(box) })
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

    private fun input(hint: String, value: String?) = EditText(this).apply {
        this.hint = hint
        setText(value ?: "")
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        isSingleLine = true
    }

    private fun padded(v: View) = LinearLayout(this).apply {
        setPadding(dp(24), dp(8), dp(24), 0)
        addView(v, matchWrap)
    }

    private fun addContact(prefill: String?) {
        // A copied invite (say, from the camera app) fills itself in.
        val field = input("threnody://… or host:port", prefill ?: copiedLink()?.takeIf { it.startsWith("threnody://") })
        AlertDialog.Builder(this)
            .setTitle("Add a contact")
            .setMessage("Scan or paste their invite. You'll check their safety number together later.")
            .setView(padded(field))
            .setPositiveButton("Connect") { _, _ -> connect(field.text.toString().trim()) }
            .setNeutralButton("Scan") { _, _ -> scan() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun scan() {
        @Suppress("DEPRECATION") // the result API needs AndroidX
        startActivityForResult(Intent(this, ScanActivity::class.java), SCAN)
    }

    @Deprecated("Activity result API needs AndroidX; this app uses the platform only.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != SCAN || resultCode != RESULT_OK) return
        val text = data?.getStringExtra(ScanActivity.RESULT)?.trim() ?: return
        when {
            text.startsWith("threnody://") -> addContact(text)
            text.startsWith("threnody-link://") -> joinAccount(text)
            else -> failed("That QR code isn't a Threnody invite or link code.")
        }
    }

    /** A Threnody invite or link code on the clipboard, if any. */
    private fun copiedLink(): String? = try {
        getSystemService(ClipboardManager::class.java)?.primaryClip
            ?.takeIf { it.itemCount > 0 }
            ?.getItemAt(0)?.coerceToText(this)?.toString()?.trim()
            ?.takeIf { it.startsWith("threnody://") || it.startsWith("threnody-link://") }
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
        val invite = link.startsWith("threnody://")
        AlertDialog.Builder(this)
            .setTitle(if (invite) "Add the copied invite?" else "Join with the copied link code?")
            .setMessage(link)
            .setPositiveButton(if (invite) "Connect" else "Continue") { _, _ ->
                if (invite) connect(link) else joinAccount(link)
            }
            .setNegativeButton("Not now", null)
            .show()
    }

    private fun connect(target: String) {
        if (target.isEmpty()) return
        val n = node ?: return
        Toast.makeText(this, "Connecting…", Toast.LENGTH_SHORT).show()
        worker.execute {
            try {
                val peer = n.connect(target)
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
        AlertDialog.Builder(this)
            .setTitle("Join another device's account")
            .setMessage("This device becomes part of that account: your contacts see both as you. " +
                "Get the code from “Link a new device” on the other device.")
            .setView(padded(field))
            .setPositiveButton("Join") { _, _ ->
                val code = field.text.toString().trim()
                val n = node ?: return@setPositiveButton
                worker.execute {
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
        }
    }

    private fun failed(msg: String) {
        AlertDialog.Builder(this).setMessage(msg).setPositiveButton("OK", null).show()
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    companion object {
        private const val SCAN = 1
    }
}
