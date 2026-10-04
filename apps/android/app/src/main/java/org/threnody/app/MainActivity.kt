package org.threnody.app

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.content.Context
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.os.Bundle
import android.text.method.ScrollingMovementMethod
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import java.net.Inet4Address
import java.util.concurrent.Executors
import uniffi.threnody_ffi.NodeEvent
import uniffi.threnody_ffi.ThrenodyNode

/**
 * The node lives for the whole process, not the Activity: rotating the
 * screen or backgrounding the app must not drop sessions. [ThrenodyService]
 * keeps the process alive; this object owns the node, the event pump, the
 * on-screen log and the Bluetooth listener.
 */
object Threnody {
    @Volatile private var instance: ThrenodyNode? = null
    var listenAddr: String = ""
        private set
    @Volatile var current: String? = null
    private val log = StringBuilder()
    /** The visible Activity's log view, or null while backgrounded. */
    @Volatile private var listener: ((String) -> Unit)? = null

    @Synchronized
    fun start(ctx: Context): ThrenodyNode = instance ?: ThrenodyNode.open(ctx.filesDir.resolve("threnody").path, null).also {
        listenAddr = it.listen("0.0.0.0:7450")
        instance = it
        say("listening on $listenAddr")
        pump(ctx.applicationContext, it)
        if (Bluetooth.canListen(ctx)) Bluetooth.start(ctx.applicationContext, it)
    }

    /** Attaches a log view; returns everything logged so far. */
    fun watch(l: ((String) -> Unit)?): String = synchronized(log) {
        listener = l
        log.toString()
    }

    fun say(line: String) {
        val l = synchronized(log) {
            log.append(line).append('\n')
            listener
        }
        l?.invoke(line)
    }

    private fun pump(ctx: Context, node: ThrenodyNode) = Thread {
        while (true) {
            when (val e = node.nextEvent(1000u)) {
                null -> {}
                is NodeEvent.Connected -> { current = current ?: e.peer; say("* connected ${short(e.peer)}") }
                is NodeEvent.Disconnected -> say("* ${short(e.peer)} disconnected (${e.reason})")
                is NodeEvent.Message -> {
                    current = current ?: e.peer
                    say("<${short(e.peer)}> ${e.text}")
                    if (listener == null) ThrenodyService.notifyMessage(ctx, short(e.peer), e.text)
                }
                is NodeEvent.ApprovalChanged -> say("* ${short(e.peer)} approval: mutual=${e.mutual}")
                is NodeEvent.File -> say("* ${short(e.peer)} sent ${e.name} (${e.data.size} bytes)")
                is NodeEvent.WifiDirectOffer -> WifiDirect.join(ctx, node, e.peer, e.ssid, e.passphrase, e.addr)
                is NodeEvent.WifiDirectRequested -> {
                    say("* ${short(e.peer)} asks for a Wi-Fi Direct link")
                    WifiDirect.host(ctx, node, e.peer)
                }
                else -> say("· $e")
            }
        }
    }.apply { isDaemon = true; name = "threnody-events" }.start()

    fun short(fp: String) = fp.take(9)
}

/** Minimal Threnody client: one node, a log, connect / send / approve. */
class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var node: ThrenodyNode
    private lateinit var log: TextView
    private var seen: List<Pair<BluetoothDevice, Int>> = emptyList()
    private var current: String?
        get() = Threnody.current
        set(v) { Threnody.current = v }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 96, 32, 32)
        }
        val header = TextView(this).apply { setTextIsSelectable(true); textSize = 13f }
        val target = EditText(this).apply { hint = "threnody://… invite or host:port" }
        val connect = Button(this).apply { text = "Connect" }
        val message = EditText(this).apply { hint = "message"; maxLines = 4 }
        val send = Button(this).apply { text = "Send" }
        val approve = Button(this).apply { text = "Approve current peer" }
        val bluetooth = Button(this).apply { text = "Start Bluetooth" }
        val scan = Button(this).apply { text = "Scan Bluetooth" }
        val direct = Button(this).apply { text = "Wi-Fi Direct with current peer" }
        val leave = Button(this).apply { text = "Leave Wi-Fi Direct" }
        log = TextView(this).apply {
            movementMethod = ScrollingMovementMethod()
            setTextIsSelectable(true)
            textSize = 13f
        }
        for (v in listOf(header, target, connect, message, send, approve, bluetooth, scan, direct, leave)) {
            root.addView(v, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        root.addView(log, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        ThrenodyService.start(this)
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 3)
        }

        worker.execute {
            try {
                node = Threnody.start(this)
                val bound = Threnody.listenAddr
                val ip = wifiIp() ?: "127.0.0.1"
                val port = bound.substringAfterLast(':')
                val invite = node.inviteLink("$ip:$port")
                runOnUiThread {
                    header.text = "device ${node.deviceFingerprint()}\n" +
                        "account ${node.accountFingerprint()}\n" +
                        "invite  $invite"
                }
            } catch (e: Exception) {
                say("! start failed: ${e.message}")
            }
        }

        connect.setOnClickListener {
            val t = target.text.toString().trim()
            if (t.startsWith("ble ")) {
                val n = t.removePrefix("ble ").trim().toIntOrNull()
                val found = n?.let { seen.getOrNull(it - 1) }
                    ?: return@setOnClickListener say("! no such Bluetooth device; scan first")
                worker.execute { Bluetooth.dial(node, found.first, found.second, null) }
                return@setOnClickListener
            }
            worker.execute {
                try { current = node.connect(t); say("* connected to ${short(current!!)}") }
                catch (e: Exception) { say("! connect: ${e.message}") }
            }
        }
        send.setOnClickListener {
            val text = message.text.toString()
            message.setText("")
            val peer = current ?: return@setOnClickListener say("! no peer yet")
            worker.execute {
                try { node.sendText(peer, text); say("<me> $text") }
                catch (e: Exception) { say("! send: ${e.message}") }
            }
        }
        scan.setOnClickListener {
            if (Bluetooth.permitted(this)) scanBluetooth()
            else requestPermissions(Bluetooth.permissions, 2)
        }
        bluetooth.setOnClickListener {
            if (Bluetooth.permitted(this)) startBluetooth()
            else requestPermissions(Bluetooth.permissions, 1)
        }
        direct.setOnClickListener {
            val peer = current ?: return@setOnClickListener say("! no peer yet")
            if (WifiDirect.permitted(this)) WifiDirect.host(applicationContext, node, peer)
            else requestPermissions(arrayOf(WifiDirect.permission), 4)
        }
        leave.setOnClickListener { WifiDirect.leave(applicationContext) }
        approve.setOnClickListener {
            val peer = current ?: return@setOnClickListener say("! no peer yet")
            worker.execute {
                try { node.setApproval(peer, true); say("* approved ${short(peer)}") }
                catch (e: Exception) { say("! approve: ${e.message}") }
            }
        }
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (code == 3 || code == 4) return
        if (results.isEmpty() || results.any { it != PackageManager.PERMISSION_GRANTED }) {
            say("! Bluetooth permission denied")
        } else if (code == 2) scanBluetooth() else startBluetooth()
    }

    private fun startBluetooth() {
        if (Bluetooth.running) return say("* Bluetooth already on")
        worker.execute { Bluetooth.start(applicationContext, node) }
    }

    private fun scanBluetooth() {
        say("* scanning Bluetooth for 8s…")
        Bluetooth.scanOnce(this, node) { found ->
            seen = found
            if (found.isEmpty()) say("* no Threnody devices nearby")
            found.forEachIndexed { i, (d, psm) -> say("  ${i + 1}: ${d.address} psm $psm — connect with \"ble ${i + 1}\"") }
        }
    }

    private fun wifiIp(): String? {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return null
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return null
        return props.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }?.hostAddress
    }

    private fun short(fp: String) = Threnody.short(fp)

    private fun say(line: String) = Threnody.say(line)

    override fun onStart() {
        super.onStart()
        val past = Threnody.watch { line -> runOnUiThread { log.append(line + "\n") } }
        log.text = past
    }

    override fun onStop() {
        // Backgrounded: incoming messages become notifications instead.
        Threnody.watch(null)
        super.onStop()
    }
}
