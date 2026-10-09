package org.threnody.app

import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Environment
import android.provider.MediaStore
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.TimeUnit
import uniffi.threnody_ffi.ContactInfo
import uniffi.threnody_ffi.FileOptions
import uniffi.threnody_ffi.NodeEvent
import uniffi.threnody_ffi.PersonaRecord
import uniffi.threnody_ffi.ThrenodyNode

/**
 * The node lives for the whole process, not an Activity: rotating the
 * screen or backgrounding the app must not drop sessions. [ThrenodyService]
 * keeps the process alive; this object owns the node, the event pump, the
 * diagnostic log and the Bluetooth listener, and fans events out to
 * whichever screens are open.
 */
object Threnody {
    @Volatile private var instance: ThrenodyNode? = null
    var listenAddr: String = ""
        private set
    private val log = StringBuilder()
    /** The diagnostics screen's log view, or null while it isn't open. */
    @Volatile private var logListener: ((String) -> Unit)? = null
    private val listeners = CopyOnWriteArrayList<(NodeEvent) -> Unit>()
    /**
     * The conversation on screen, if any: the devices of a contact, or a
     * group id. Its messages don't notify.
     */
    @Volatile var visibleChat: Set<String> = emptySet()
    /** Number of started Activities; zero means the app is in the background. */
    @Volatile var visible = 0
        set(v) {
            val was = field > 0
            field = v
            // Contacts are looked for more often while the app is open.
            if ((v > 0) != was) {
                instance?.setForeground(v > 0)
                updateStandby()
            }
        }

    @Synchronized
    fun start(ctx: Context): ThrenodyNode = instance ?: open(ctx).also {
        debuggable = ctx.applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE != 0
        if (debuggable) debugHooks(ctx.applicationContext, it)
        listenAddr = it.listen("0.0.0.0:7450")
        instance = it
        say("listening on $listenAddr")
        pump(ctx.applicationContext, it, null)
        if (Bluetooth.canListen(ctx)) Bluetooth.start(ctx.applicationContext, it)
        openPersonas(ctx.applicationContext, it)
        applyPrivacy(ctx)
        redialOnNetwork(ctx.applicationContext, it)
        keepStandby(ctx.applicationContext, it)
        nameThisDevice(it)
        sweepMedia(ctx.applicationContext, it)
    }

    /** Anonymous identities' nodes, by persona id; each runs alongside the main one. */
    private val personaNodes = java.util.concurrent.ConcurrentHashMap<String, ThrenodyNode>()
    /** Their labels (the user's own, never sent), by persona id. */
    @Volatile var personaLabels: Map<String, String> = emptyMap()
        private set

    /** The node for a conversation: the main identity's, or a persona's. */
    fun node(ctx: Context, persona: String?): ThrenodyNode {
        val main = start(ctx)
        return if (persona == null) main else personaNodes[persona]
            ?: throw IllegalStateException("that anonymous identity no longer exists")
    }

    fun personaNode(id: String): ThrenodyNode? = personaNodes[id]
    fun personaIds(): List<String> = personaNodes.keys().toList()

    /** Burns expired personas, then starts the rest. */
    private fun openPersonas(ctx: Context, main: ThrenodyNode) {
        try {
            for (id in main.burnExpiredPersonas()) forgetPersona(ctx, id)
            for (rec in main.personas()) openPersona(ctx, rec)
        } catch (e: Exception) {
            say("! anonymous identities: ${e.message}")
        }
        refreshLabels(main)
    }

    private fun refreshLabels(main: ThrenodyNode) {
        personaLabels = try { main.personas().associate { it.id to it.label } } catch (_: Exception) { emptyMap() }
    }

    private fun prefs(ctx: Context) = ctx.getSharedPreferences("personas", Context.MODE_PRIVATE)

    /**
     * Opens a persona's node: sealed with the same Keystore key, listening
     * on a port of its own (kept, so its invites keep working). No
     * Bluetooth or discovery: those would show nearby devices who it is.
     */
    @Synchronized
    private fun openPersona(ctx: Context, rec: PersonaRecord): ThrenodyNode {
        personaNodes[rec.id]?.let { return it }
        val n = ThrenodyNode.open(rec.home, KeyVault.passphrase(ctx, rec.home), null)
        val port = prefs(ctx).getInt("port_${rec.id}", 0)
        val addr = try { n.listen("0.0.0.0:$port") } catch (_: Exception) { n.listen("0.0.0.0:0") }
        prefs(ctx).edit().putInt("port_${rec.id}", addr.substringAfterLast(':').toInt()).apply()
        personaNodes[rec.id] = n
        applyPrivacyTo(ctx, n)
        pump(ctx, n, rec.id)
        say("anonymous identity ${rec.id.take(6)} listening on port ${addr.substringAfterLast(':')}")
        // The same directories as the main identity, through links of its own.
        Thread {
            try { DirectoryUi.followMain(ctx, n) } catch (e: Exception) { say("! followMain: ${e.message}") }
        }.apply { isDaemon = true }.start()
        return n
    }

    /** The port a persona listens on, for its invites. */
    fun personaPort(ctx: Context, id: String): Int = prefs(ctx).getInt("port_$id", 0)

    /** Makes and starts a new anonymous identity. */
    fun createPersona(ctx: Context, label: String, expiresMs: Long?): PersonaRecord {
        val main = start(ctx)
        val rec = main.createPersona(label, expiresMs?.toULong(), KeyVault.secret(ctx))
        say("* new anonymous identity ${rec.id.take(6)}" + (rec.expiresMs?.let { ", burns at $it" } ?: ", kept until burned"))
        openPersona(ctx.applicationContext, rec)
        refreshLabels(main)
        return rec
    }

    /** Burns an anonymous identity: its node, keys, history and files. */
    fun burnPersona(ctx: Context, id: String) {
        val main = start(ctx)
        personaNodes.remove(id)?.shutdown()
        main.burnPersona(id)
        forgetPersona(ctx, id)
        refreshLabels(main)
    }

    fun renamePersona(ctx: Context, id: String, label: String) {
        val main = start(ctx)
        main.renamePersona(id, label)
        refreshLabels(main)
    }

    private fun forgetPersona(ctx: Context, id: String) {
        Media.dir(ctx, id).deleteRecursively()
        prefs(ctx).edit().remove("port_$id").apply()
        say("* burned anonymous identity ${id.take(6)}")
    }

    /** Removes kept photos whose messages were deleted or disappeared. */
    private fun sweepMedia(ctx: Context, node: ThrenodyNode) {
        Threading.schedulePeriodic(1, 5, TimeUnit.MINUTES) {
            Media.sweep(ctx, node)
            for ((id, n) in personaNodes) Media.sweep(ctx, n, id)
            // Expired personas burn while the app runs, too.
            try {
                val due = node.personas().filter { p -> p.expiresMs?.let { it.toLong() <= System.currentTimeMillis() } == true }
                for (p in due) burnPersona(ctx, p.id)
            } catch (e: Exception) {
                say("! persona expiry check: ${e.message}")
            }
        }
    }

    /** Applies the privacy settings (all on unless turned off) to the node. */
    fun applyPrivacy(ctx: Context) {
        instance?.let { applyPrivacyTo(ctx, it) }
        for (n in personaNodes.values) applyPrivacyTo(ctx, n)
    }

    private fun applyPrivacyTo(ctx: Context, node: ThrenodyNode) {
        node.setCoverTraffic(Privacy.coverMs(ctx))
        node.setOnionFirst(Privacy.onionFirst(ctx))
        node.setDefaultDisappearing(Privacy.defaultTimer(ctx))
        node.setStripMetadata(Privacy.stripMetadata(ctx))
        node.setReachInternet(Privacy.reachInternet(ctx))
        node.setUseVolunteers(Privacy.useVolunteers(ctx))
    }

    /** What a notification or the chat list says for a file. */
    fun fileLabel(name: String, sensitive: Boolean, caption: String): String = when {
        sensitive && Media.isImage(name) -> "📷 Sensitive photo"
        sensitive -> "📎 Sensitive file"
        Media.isImage(name) -> "📷 " + caption.ifBlank { "Photo" }
        else -> "📎 " + caption.ifBlank { name }
    }

    /**
     * Keeps a received file: images privately, others in Downloads. A
     * persona keeps everything privately, so burning it leaves nothing.
     */
    private fun keep(ctx: Context, name: String, data: ByteArray, persona: String?): String? =
        if (persona != null || Media.isImage(name)) Media.savePrivate(ctx, name, data, persona)
        else saveDownload(ctx, name, data)?.toString()

    /**
     * New identities are called "device"; give this one the phone's model
     * name (its siblings and contacts see it). A name the user chose stays.
     */
    private fun nameThisDevice(node: ThrenodyNode) {
        val me = node.devices().firstOrNull { it.thisDevice } ?: return
        if (me.name != "device") return
        val model = listOf(android.os.Build.MANUFACTURER, android.os.Build.MODEL)
            .let { (maker, model) -> if (model.startsWith(maker, ignoreCase = true)) model else "$maker $model" }
            .replaceFirstChar { it.uppercase() }
        try {
            node.renameDevice(me.fingerprint, model)
            say("* named this device \"$model\"")
        } catch (e: Exception) {
            say("! naming this device: ${e.message}")
        }
    }

    private val redialer: java.util.concurrent.ScheduledExecutorService = Threading.scheduled
    /** Seconds between redial attempts; the last repeats. */
    private val redialDelays = longArrayOf(3, 10, 30, 60, 120, 300)
    @Volatile private var redialing = false

    /**
     * After an approved contact's session ends, redials with backoff until
     * every approved contact is connected again. A session can end without
     * any network change (a Wi-Fi Direct group closing, a peer restarting),
     * so the network callback alone isn't enough.
     */
    @Synchronized
    private fun redialSoon(node: ThrenodyNode) {
        if (redialing) return
        redialing = true
        fun attempt(n: Int) {
            redialer.schedule({
                val missing = node.contacts().any { it.mutuallyApproved && !it.connected }
                if (!missing) {
                    redialing = false
                    return@schedule
                }
                say("* redialing approved contacts")
                node.reconnect()
                attempt(n + 1)
            }, redialDelays[minOf(n, redialDelays.size - 1)], TimeUnit.SECONDS)
        }
        attempt(0)
    }

    /** The default network is Wi-Fi (else mobile data, or none). */
    @Volatile private var onWifi = false
    /** The Wi-Fi signal is weak enough that losing it soon is likely. */
    @Volatile private var wifiWeak = false
    /** When a message last came or went, for the standby policy. */
    @Volatile private var lastActivity = 0L
    /** Below this Wi-Fi signal (dBm), the standby path is kept warm. */
    private const val WEAK_WIFI_DBM = -72
    /** A chat counts as active for this long after its last message. */
    private const val ACTIVE_MS = 10 * 60_000L

    /** A message came or went: keep the standby path warm for a while. */
    fun touch() {
        lastActivity = System.currentTimeMillis()
        updateStandby()
    }

    /**
     * Keeps holes open through mobile data while on Wi-Fi, so losing Wi-Fi
     * costs a fraction of a second, but only while it matters (the radio
     * time costs battery): the app is open, a chat was active in the last
     * ten minutes, or the Wi-Fi signal is weakening.
     */
    fun updateStandby() {
        val n = instance ?: return
        // A standby on Wi-Fi (just joined, not default yet) costs nothing
        // to keep warm; one on mobile data costs radio time.
        val standbyWifi = standbyNet?.let { nets[it] } == true
        val warm = forceWarm ?: (standbyWifi || (onWifi && (visible > 0 || wifiWeak ||
            System.currentTimeMillis() - lastActivity < ACTIVE_MS)))
        n.setStandbyWarm(warm)
    }

    /** The default network, as last reported. */
    @Volatile private var defaultNet: Network? = null
    /** The network the standby socket is bound to, if any. */
    @Volatile private var standbyNet: Network? = null
    /** Wi-Fi and mobile networks that are up: true for Wi-Fi. */
    private val nets = java.util.concurrent.ConcurrentHashMap<Network, Boolean>()

    /**
     * Keeps a socket bound to whichever network is up but isn't the
     * default: mobile data while on Wi-Fi (for when Wi-Fi goes), or a Wi-Fi
     * that just connected while on mobile data (Android switches to it a
     * moment later; the path is ready by then). `network_changed` dials
     * through it the moment it becomes the default.
     */
    private fun keepStandby(ctx: Context, node: ThrenodyNode) {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return
        for ((transport, wifi) in listOf(
            NetworkCapabilities.TRANSPORT_CELLULAR to false,
            NetworkCapabilities.TRANSPORT_WIFI to true,
        )) {
            val req = android.net.NetworkRequest.Builder()
                .addTransportType(transport)
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build()
            cm.requestNetwork(req, object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    nets[network] = wifi
                    chooseStandby(cm, node)
                }

                override fun onLost(network: Network) {
                    nets.remove(network)
                    chooseStandby(cm, node)
                }
            })
        }
        Threading.schedulePeriodic(1, 1, TimeUnit.MINUTES) { updateStandby() }
    }

    /** Binds the standby socket to a network that is up and not the default. */
    @Synchronized
    private fun chooseStandby(cm: ConnectivityManager, node: ThrenodyNode) {
        val want = nets.keys.firstOrNull { it != defaultNet }
        if (want == standbyNet) return
        standbyNet = want
        if (want == null) {
            node.standbyLost()
            say("* no standby path")
            updateStandby()
            return
        }
        try {
            val fd = node.standbySocket()
            android.os.ParcelFileDescriptor.fromFd(fd).use { want.bindSocket(it.fileDescriptor) }
            val v6 = cm.getLinkProperties(want)?.linkAddresses.orEmpty()
                .map { it.address }
                .filterIsInstance<java.net.Inet6Address>()
                .filter { !it.isLinkLocalAddress && !it.isSiteLocalAddress && !it.isLoopbackAddress }
                .mapNotNull { it.hostAddress?.substringBefore('%') }
            node.standbyBound(v6)
            say("* standby path on ${if (nets[want] == true) "Wi-Fi" else "mobile data"} ready")
        } catch (e: Exception) {
            standbyNet = null
            say("! standby path: ${e.message}")
        }
        updateStandby()
    }

    /**
     * Redials approved contacts whenever a network becomes available,
     * including right away if one already is.
     */
    private fun redialOnNetwork(ctx: Context, node: ThrenodyNode) {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return
        cm.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                val caps = cm.getNetworkCapabilities(network)
                val cellular = caps?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true
                onWifi = caps?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
                defaultNet = network
                // First, before anything else: this side knows it moved,
                // and may already have a path open on the new network.
                node.networkChanged(network == standbyNet, cellular)
                // The standby moves to the other network, if one is up.
                chooseStandby(cm, node)
                say("* network available (${if (cellular) "mobile data" else if (onWifi) "Wi-Fi" else "other"}); reconnecting")
                node.reconnect()
                for (n in personaNodes.values) n.reconnect()
            }

            // Cover traffic stays on; on metered data it runs slower.
            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
                if (caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
                    val weak = caps.signalStrength != NetworkCapabilities.SIGNAL_STRENGTH_UNSPECIFIED &&
                        caps.signalStrength < WEAK_WIFI_DBM
                    if (weak != wifiWeak) {
                        wifiWeak = weak
                        updateStandby()
                    }
                }
                val metered = !caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
                if (metered != Privacy.metered) {
                    Privacy.metered = metered
                    applyPrivacy(ctx)
                    say("* ${if (metered) "metered" else "unmetered"} network: cover traffic every ${Privacy.coverMs(ctx) ?: "—"} ms")
                }
            }
        })
    }

    /** Opens the node, its identity sealed under the Android Keystore. */
    private fun open(ctx: Context): ThrenodyNode {
        Calls.prepare(ctx)
        val home = ctx.filesDir.resolve("threnody").path
        return ThrenodyNode.open(home, KeyVault.passphrase(ctx, home), null)
    }

    /** Subscribes to node events (called on the event thread); returns an unsubscriber. */
    fun subscribe(l: (NodeEvent) -> Unit): () -> Unit {
        listeners.add(l)
        return { listeners.remove(l) }
    }

    /** Attaches a log view; returns everything logged so far. */
    fun watchLog(l: ((String) -> Unit)?): String = synchronized(log) {
        logListener = l
        log.toString()
    }

    /** Debug builds also copy the log to logcat, for testing over adb. */
    @Volatile private var debuggable = false

    /** Forced standby warmth, for testing (null: the normal policy). */
    @Volatile private var forceWarm: Boolean? = null

    /**
     * Debug builds only: test commands over adb, so tests don't need the
     * screen. `adb shell am broadcast -a org.threnody.app.DEBUG --es cmd
     * "keepalive 60"` (or "warm on", "warm off", "warm auto").
     */
    private fun debugHooks(ctx: Context, node: ThrenodyNode) {
        val receiver = object : android.content.BroadcastReceiver() {
            override fun onReceive(c: Context, i: android.content.Intent) {
                val cmd = i.getStringExtra("cmd")?.trim() ?: return
                val arg = cmd.substringAfter(' ', "")
                when (cmd.substringBefore(' ')) {
                    "keepalive" -> arg.toUIntOrNull()?.let { node.setStandbyKeepalive(it) }
                    "warm" -> forceWarm = when (arg) { "on" -> true; "off" -> false; else -> null }
                }
                updateStandby()
                say("* debug: $cmd")
            }
        }
        val filter = android.content.IntentFilter("org.threnody.app.DEBUG")
        if (android.os.Build.VERSION.SDK_INT >= 33) {
            ctx.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            ctx.registerReceiver(receiver, filter)
        }
    }

    fun say(line: String) {
        if (debuggable) android.util.Log.d("Threnody", line)
        val l = synchronized(log) {
            log.append(line).append('\n')
            logListener
        }
        l?.invoke(line)
    }

    /**
     * The conversation a device belongs to: its account when known, so all
     * of a contact's devices share one chat, else the device itself.
     */
    fun key(contacts: List<ContactInfo>, device: String): String =
        contacts.firstOrNull { it.fingerprint == device }?.account ?: device

    /** One entry per conversation, preferring a connected device. */
    fun conversations(node: ThrenodyNode, persona: String? = null): List<Conversation> =
        node.contacts().groupBy { it.account ?: it.fingerprint }.map { (k, devices) ->
            val best = devices.firstOrNull { it.connected } ?: devices.first()
            Conversation(
                key = k,
                device = best.fingerprint,
                name = devices.firstNotNullOfOrNull { it.name },
                connected = devices.any { it.connected },
                approved = devices.any { it.localApproved },
                verified = devices.all { it.verified },
                unverified = devices.filter { !it.verified }.map { it.fingerprint },
                accepted = devices.any { it.accepted },
                blocked = devices.any { it.blocked },
                anyVerified = devices.any { it.verified },
                devices = devices.map { it.fingerprint },
                persona = persona,
                revealed = devices.firstNotNullOfOrNull { it.revealed },
                revealedInvite = devices.firstNotNullOfOrNull { it.revealedInvite },
            )
        }

    private fun pump(ctx: Context, node: ThrenodyNode, persona: String?) = Thread {
        while (true) {
            val e = node.nextEvent(1000u) ?: continue
            when (e) {
                is NodeEvent.Connected -> say("* connected ${short(e.peer)}" + (e.via?.let { " via ${short(it)}" } ?: ""))
                is NodeEvent.Disconnected -> {
                    say("* ${short(e.peer)} disconnected (${e.reason})")
                    if (node.contacts().any { it.fingerprint == e.peer && it.mutuallyApproved }) redialSoon(node)
                }
                is NodeEvent.Message -> {
                    touch()
                    // The log is for transports, not content.
                    say("* message from ${short(e.peer)} (${e.text.length} chars)")
                    val contacts = node.contacts()
                    val k = key(contacts, e.peer)
                    if (visible == 0 || e.peer !in visibleChat) {
                        val name = contacts.firstOrNull { it.fingerprint == e.peer }?.name ?: short(e.peer)
                        ThrenodyService.notifyMessage(ctx, k, e.peer, name, e.text, persona)
                    }
                }
                is NodeEvent.ApprovalChanged -> say("* ${short(e.peer)} approval: mutual=${e.mutual}")
                is NodeEvent.File -> {
                    say("* ${short(e.peer)} sent a file (${e.data.size} bytes)")
                    val location = keep(ctx, e.name, e.data, persona)
                    try {
                        node.recordReceivedFile(e.peer, e.name, e.data.size.toULong(), location, e.id,
                            FileOptions(e.sensitive, e.caption, e.album))
                    } catch (x: Exception) {
                        say("! recording a file: ${x.message}")
                    }
                    if (visible == 0 || e.peer !in visibleChat) {
                        val contacts = node.contacts()
                        ThrenodyService.notifyMessage(ctx, key(contacts, e.peer), e.peer, nameOf(node, e.peer),
                            fileLabel(e.name, e.sensitive, e.caption), persona)
                    }
                }
                is NodeEvent.GroupMessage -> {
                    say("* group ${e.group.take(6)}: message from ${short(e.from)} (${e.text.length} chars)")
                    // Our own other device's messages aren't news.
                    if (!e.ours && (visible == 0 || e.group !in visibleChat)) {
                        val group = node.groups().firstOrNull { it.id == e.group }
                        ThrenodyService.notifyGroup(ctx, e.group, group?.name ?: "Group", "${nameOf(node, e.from)}: ${e.text}", persona)
                    }
                }
                is NodeEvent.GroupFile -> {
                    say("* group ${e.group.take(6)}: ${short(e.from)} sent a file (${e.data.size} bytes)")
                    val location = keep(ctx, e.name, e.data, persona)
                    try {
                        node.recordReceivedGroupFile(e.group, e.from, e.name, e.data.size.toULong(), location, e.id,
                            FileOptions(e.sensitive, e.caption, e.album))
                    } catch (x: Exception) {
                        say("! recording a file: ${x.message}")
                    }
                    if (!e.ours && (visible == 0 || e.group !in visibleChat)) {
                        val group = node.groups().firstOrNull { it.id == e.group }
                        ThrenodyService.notifyGroup(ctx, e.group, group?.name ?: "Group",
                            "${nameOf(node, e.from)}: ${fileLabel(e.name, e.sensitive, e.caption)}", persona)
                    }
                }
                is NodeEvent.GroupInvited -> {
                    say("* ${short(e.from)} invites us to group ${e.name}")
                    ThrenodyService.notifyGroup(ctx, e.group, e.name, "${nameOf(node, e.from)} invites you to join", persona)
                }
                is NodeEvent.CredentialOffered -> {
                    say("* ${short(e.offer.peer)} offers a credential (${e.offer.schema})")
                    if (visible == 0 || e.offer.peer !in visibleChat) {
                        val contacts = node.contacts()
                        ThrenodyService.notifyMessage(ctx, key(contacts, e.offer.peer), e.offer.peer,
                            nameOf(node, e.offer.peer), "Offers you a credential", persona)
                    }
                }
                is NodeEvent.CredentialAsked -> {
                    say("* ${short(e.ask.peer)} asks for a proof (${e.ask.schema})")
                    if (visible == 0 || e.ask.peer !in visibleChat) {
                        val contacts = node.contacts()
                        ThrenodyService.notifyMessage(ctx, key(contacts, e.ask.peer), e.ask.peer,
                            nameOf(node, e.ask.peer), "Asks you to prove something", persona)
                    }
                }
                is NodeEvent.VolunteerNote -> say("* ${e.note}")
                is NodeEvent.WifiDirectOffer -> WifiDirect.join(ctx, node, e.peer, e.ssid, e.passphrase, e.addr)
                is NodeEvent.WifiDirectRequested -> {
                    say("* ${short(e.peer)} asks for a Wi-Fi Direct link")
                    WifiDirect.host(ctx, node, e.peer)
                }
                is NodeEvent.MessageRequest -> {
                    say("* message request from ${short(e.peer)}")
                    val k = key(node.contacts(), e.peer)
                    if (visible == 0 || e.peer !in visibleChat) {
                        // Not even who: a stranger's name can be a message in itself.
                        ThrenodyService.notifyMessage(ctx, k, e.peer, "Threnody", "New message request", persona)
                    }
                }
                is NodeEvent.IdentityRevealed -> {
                    say("* ${short(e.peer)} revealed their identity")
                    val c = conversations(node, persona).firstOrNull { e.peer in it.devices }
                    if (c != null) {
                        ThrenodyService.notifyMessage(ctx, c.key, c.device, c.title,
                            "Proved who they are. Open the chat to add them.", persona)
                    }
                }
                is NodeEvent.ThisDeviceRemoved -> {
                    say("! this device was removed from its account")
                    ThrenodyService.notifyAccount(ctx, "This device was removed from your account. " +
                        "Your contacts no longer accept it as you.")
                }
                is NodeEvent.AccountChanged -> {
                    say("· $e")
                    // A verified contact gained a device we haven't verified.
                    if (e.added.isNotEmpty() && e.account != node.accountFingerprint()) {
                        val c = conversations(node).firstOrNull { c -> e.added.any { it in c.devices } }
                        if (c != null && c.anyVerified) {
                            ThrenodyService.notifyMessage(ctx, c.key, c.device, "Safety alert: ${c.title}",
                                "${c.title} added a device you haven't verified. Compare safety numbers.")
                        }
                    }
                }
                is NodeEvent.CallIncoming, is NodeEvent.CallRinging, is NodeEvent.CallStarted,
                is NodeEvent.CallMedia, is NodeEvent.CallVideo, is NodeEvent.CallReaction, is NodeEvent.CallEnded -> {
                    // The log is for transports: that a call happened, not with whom.
                    if (e is NodeEvent.CallIncoming || e is NodeEvent.CallEnded) say("* call: ${e::class.simpleName}")
                    Calls.onEvent(ctx, node, persona, e)
                    if (e is NodeEvent.CallIncoming && visible > 0) {
                        ctx.startActivity(Intent(ctx, CallActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                    }
                }
                else -> say("· $e")
            }
            for (l in listeners) l(e)
        }
    }.apply { isDaemon = true; name = "threnody-events" }.start()

    /** A contact's name, "You" for this device, else a short fingerprint. */
    fun nameOf(node: ThrenodyNode, fp: String): String = when (fp) {
        node.deviceFingerprint() -> "You"
        else -> node.contacts().firstOrNull { it.fingerprint == fp }?.name ?: short(fp)
    }

    /** Saves a received file to Downloads/Threnody; null if that fails. */
    private fun saveDownload(ctx: Context, name: String, data: ByteArray): Uri? = try {
        val values = ContentValues().apply {
            // Strip any path the sender put in the name.
            put(MediaStore.Downloads.DISPLAY_NAME, name.substringAfterLast('/').ifBlank { "file" })
            put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/Threnody")
        }
        val r = ctx.contentResolver
        r.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)?.also { uri ->
            r.openOutputStream(uri)?.use { it.write(data) }
        }
    } catch (e: Exception) {
        say("! saving $name: ${e.message}")
        null
    }

    fun short(fp: String) = fp.take(9)
}

/** A chat partner: one account (or lone device) and how to reach it. */
data class Conversation(
    val key: String,
    /** The device to address; sends reach every device of the account. */
    val device: String,
    val name: String?,
    val connected: Boolean,
    val approved: Boolean,
    val verified: Boolean,
    /** Devices whose safety number hasn't been compared. */
    val unverified: List<String>,
    /** Some device was verified: an unverified one is new (a safety alert). */
    val anyVerified: Boolean,
    /** We want their messages; otherwise this is a message request. */
    val accepted: Boolean,
    val blocked: Boolean,
    /** Every device of the account that we know. */
    val devices: List<String>,
    /** The anonymous identity this conversation belongs to; null for the main one. */
    val persona: String? = null,
    /** The identity they proved they are (they reached us anonymously), and how to reach it. */
    val revealed: String? = null,
    val revealedInvite: String? = null,
) {
    val title get() = name ?: Threnody.short(device)
}
