package org.threnody.app

import android.content.ContentValues
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Environment
import android.provider.MediaStore
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import uniffi.threnody_ffi.ContactInfo
import uniffi.threnody_ffi.NodeEvent
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

    @Synchronized
    fun start(ctx: Context): ThrenodyNode = instance ?: open(ctx).also {
        listenAddr = it.listen("0.0.0.0:7450")
        instance = it
        say("listening on $listenAddr")
        pump(ctx.applicationContext, it)
        if (Bluetooth.canListen(ctx)) Bluetooth.start(ctx.applicationContext, it)
        applyPrivacy(ctx)
        redialOnNetwork(ctx.applicationContext, it)
        nameThisDevice(it)
    }

    /** Applies the privacy settings (all on unless turned off) to the node. */
    fun applyPrivacy(ctx: Context) {
        val node = instance ?: return
        node.setCoverTraffic(Privacy.coverMs(ctx))
        node.setOnionFirst(Privacy.onionFirst(ctx))
        node.setDefaultDisappearing(Privacy.defaultTimer(ctx))
    }

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

    private val redialer = Executors.newSingleThreadScheduledExecutor { r ->
        Thread(r, "threnody-redial").apply { isDaemon = true }
    }
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

    /**
     * Redials approved contacts whenever a network becomes available,
     * including right away if one already is.
     */
    private fun redialOnNetwork(ctx: Context, node: ThrenodyNode) {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return
        cm.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                say("* network available; reconnecting to approved contacts")
                node.reconnect()
            }

            // Cover traffic stays on; on metered data it runs slower.
            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
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
        val home = ctx.filesDir.resolve("threnody").path
        return ThrenodyNode.open(home, KeyVault.passphrase(ctx, home))
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

    fun say(line: String) {
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
    fun conversations(node: ThrenodyNode): List<Conversation> =
        node.contacts().groupBy { it.account ?: it.fingerprint }.map { (k, devices) ->
            val best = devices.firstOrNull { it.connected } ?: devices.first()
            Conversation(
                key = k,
                device = best.fingerprint,
                name = devices.firstNotNullOfOrNull { it.name },
                connected = devices.any { it.connected },
                approved = devices.any { it.mutuallyApproved },
                verified = devices.all { it.verified },
                devices = devices.map { it.fingerprint },
            )
        }

    private fun pump(ctx: Context, node: ThrenodyNode) = Thread {
        while (true) {
            val e = node.nextEvent(1000u) ?: continue
            when (e) {
                is NodeEvent.Connected -> say("* connected ${short(e.peer)}" + (e.via?.let { " via ${short(it)}" } ?: ""))
                is NodeEvent.Disconnected -> {
                    say("* ${short(e.peer)} disconnected (${e.reason})")
                    if (node.contacts().any { it.fingerprint == e.peer && it.mutuallyApproved }) redialSoon(node)
                }
                is NodeEvent.Message -> {
                    // The log is for transports, not content.
                    say("* message from ${short(e.peer)} (${e.text.length} chars)")
                    val contacts = node.contacts()
                    val k = key(contacts, e.peer)
                    if (visible == 0 || e.peer !in visibleChat) {
                        val name = contacts.firstOrNull { it.fingerprint == e.peer }?.name ?: short(e.peer)
                        ThrenodyService.notifyMessage(ctx, k, e.peer, name, e.text)
                    }
                }
                is NodeEvent.ApprovalChanged -> say("* ${short(e.peer)} approval: mutual=${e.mutual}")
                is NodeEvent.File -> {
                    say("* ${short(e.peer)} sent ${e.name} (${e.data.size} bytes)")
                    val uri = saveDownload(ctx, e.name, e.data)
                    try {
                        node.recordReceivedFile(e.peer, e.name, e.data.size.toULong(), uri?.toString())
                    } catch (x: Exception) {
                        say("! recording ${e.name}: ${x.message}")
                    }
                    if (visible == 0 || e.peer !in visibleChat) {
                        val contacts = node.contacts()
                        ThrenodyService.notifyMessage(ctx, key(contacts, e.peer), e.peer, nameOf(node, e.peer), "📎 ${e.name}")
                    }
                }
                is NodeEvent.GroupMessage -> {
                    say("* group ${e.group.take(6)}: message from ${short(e.from)} (${e.text.length} chars)")
                    // Our own other device's messages aren't news.
                    if (!e.ours && (visible == 0 || e.group !in visibleChat)) {
                        val group = node.groups().firstOrNull { it.id == e.group }
                        ThrenodyService.notifyGroup(ctx, e.group, group?.name ?: "Group", "${nameOf(node, e.from)}: ${e.text}")
                    }
                }
                is NodeEvent.GroupInvited -> {
                    say("* ${short(e.from)} invites us to group ${e.name}")
                    ThrenodyService.notifyGroup(ctx, e.group, e.name, "${nameOf(node, e.from)} invites you to join")
                }
                is NodeEvent.WifiDirectOffer -> WifiDirect.join(ctx, node, e.peer, e.ssid, e.passphrase, e.addr)
                is NodeEvent.WifiDirectRequested -> {
                    say("* ${short(e.peer)} asks for a Wi-Fi Direct link")
                    WifiDirect.host(ctx, node, e.peer)
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
    /** Every device of the account that we know. */
    val devices: List<String>,
) {
    val title get() = name ?: Threnody.short(device)
}
