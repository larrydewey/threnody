package org.threnody.app

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.ParcelUuid
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
import java.util.UUID
import java.util.concurrent.Executors
import uniffi.threnody_ffi.ByteLink
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
    @Volatile var bleServer: BluetoothServerSocket? = null
    private val log = StringBuilder()
    /** The visible Activity's log view, or null while backgrounded. */
    @Volatile private var listener: ((String) -> Unit)? = null

    @Synchronized
    fun start(ctx: Context): ThrenodyNode = instance ?: ThrenodyNode.open(ctx.filesDir.resolve("threnody").path, null).also {
        listenAddr = it.listen("0.0.0.0:7450")
        instance = it
        say("listening on $listenAddr")
        pump(ctx.applicationContext, it)
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
        val message = EditText(this).apply { hint = "message" }
        val send = Button(this).apply { text = "Send" }
        val approve = Button(this).apply { text = "Approve current peer" }
        val bluetooth = Button(this).apply { text = "Start Bluetooth" }
        val scan = Button(this).apply { text = "Scan Bluetooth" }
        log = TextView(this).apply {
            movementMethod = ScrollingMovementMethod()
            setTextIsSelectable(true)
            textSize = 13f
        }
        for (v in listOf(header, target, connect, message, send, approve, bluetooth, scan)) {
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
                worker.execute { dial(found.first, found.second) }
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
            val needed = arrayOf(Manifest.permission.BLUETOOTH_CONNECT, Manifest.permission.BLUETOOTH_SCAN)
                .filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
            if (needed.isEmpty()) scanBluetooth() else requestPermissions(needed.toTypedArray(), 2)
        }
        bluetooth.setOnClickListener {
            val needed = arrayOf(Manifest.permission.BLUETOOTH_CONNECT, Manifest.permission.BLUETOOTH_ADVERTISE)
                .filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
            if (needed.isEmpty()) startBluetooth() else requestPermissions(needed.toTypedArray(), 1)
        }
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
        if (code == 3) return
        if (results.isEmpty() || results.any { it != PackageManager.PERMISSION_GRANTED }) {
            say("! Bluetooth permission denied")
        } else if (code == 2) scanBluetooth() else startBluetooth()
    }

    /**
     * Listens on an L2CAP channel and advertises its PSM under the Threnody
     * service UUID. The channel is "insecure" at the Bluetooth layer on
     * purpose: no pairing is needed, the Threnody handshake authenticates.
     */
    @Suppress("MissingPermission")
    private fun startBluetooth() {
        if (Threnody.bleServer != null) return say("* Bluetooth already on")
        val adapter = getSystemService(BluetoothManager::class.java)?.adapter
            ?: return say("! no Bluetooth adapter")
        val server = try { adapter.listenUsingInsecureL2capChannel() }
            catch (e: Exception) { return say("! L2CAP listen: ${e.message}") }
        Threnody.bleServer = server
        val psm = server.psm
        val data = AdvertiseData.Builder()
            .addServiceData(ParcelUuid(UUID.fromString(BLE_SERVICE)), byteArrayOf((psm and 0xff).toByte(), (psm shr 8).toByte()))
            .setIncludeDeviceName(false)
            .build()
        val settings = AdvertiseSettings.Builder()
            .setConnectable(true)
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_HIGH)
            .build()
        adapter.bluetoothLeAdvertiser?.startAdvertising(settings, data, object : AdvertiseCallback() {
            override fun onStartSuccess(s: AdvertiseSettings?) = say("* Bluetooth: advertising, L2CAP psm $psm")
            override fun onStartFailure(code: Int) = say("! Bluetooth advertise failed ($code)")
        }) ?: say("! BLE advertising not supported")
        Thread {
            while (true) {
                val sock = try { server.accept() } catch (e: Exception) { break }
                say("* Bluetooth link from ${sock.remoteDevice.address}")
                attach(sock)
            }
        }.start()
    }

    /** Scans for Threnody adverts for 8 seconds and lists them. */
    @Suppress("MissingPermission")
    private fun scanBluetooth() {
        val scanner = getSystemService(BluetoothManager::class.java)?.adapter?.bluetoothLeScanner
            ?: return say("! no Bluetooth scanner")
        val uuid = ParcelUuid(UUID.fromString(BLE_SERVICE))
        val found = LinkedHashMap<String, Pair<BluetoothDevice, Int>>()
        val cb = object : ScanCallback() {
            override fun onScanResult(type: Int, r: ScanResult) {
                val data = r.scanRecord?.getServiceData(uuid) ?: return
                if (data.size < 2) return
                val psm = (data[0].toInt() and 0xff) or ((data[1].toInt() and 0xff) shl 8)
                found[r.device.address] = r.device to psm
            }
        }
        say("* scanning Bluetooth for 8s…")
        val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build()
        scanner.startScan(null, settings, cb)
        log.postDelayed({
            scanner.stopScan(cb)
            seen = found.values.toList()
            if (seen.isEmpty()) say("* no Threnody devices nearby")
            seen.forEachIndexed { i, (d, psm) -> say("  ${i + 1}: ${d.address} psm $psm — connect with \"ble ${i + 1}\"") }
        }, 8000)
    }

    /**
     * Opens an L2CAP channel to a scanned device and starts a session.
     * LE connection setup fails transiently (HCI 0x3e) often enough that a
     * few attempts are worth making.
     */
    @Suppress("MissingPermission")
    private fun dial(device: BluetoothDevice, psm: Int) {
        say("* dialing ${device.address} psm $psm…")
        for (attempt in 1..3) {
            try {
                val sock = device.createInsecureL2capChannel(psm)
                sock.connect()
                attach(sock, outbound = true)
                return
            } catch (e: Exception) {
                if (attempt == 3) say("! Bluetooth dial: ${e.message}")
                else Thread.sleep(500)
            }
        }
    }

    /** Bridges one Bluetooth socket into the node. */
    private fun attach(sock: BluetoothSocket, outbound: Boolean = false) {
        val out = sock.outputStream
        val link = object : ByteLink {
            override fun send(data: ByteArray): Boolean =
                try { out.write(data); out.flush(); true } catch (e: Exception) { false }
            override fun disconnect() { try { sock.close() } catch (_: Exception) {} }
        }
        val handle = node.attachLink(link, outbound, "ble", sock.remoteDevice.address, null)
        Thread {
            val buf = ByteArray(16 * 1024)
            try {
                val input = sock.inputStream
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    handle.receive(buf.copyOf(n))
                }
            } catch (_: Exception) {
            } finally {
                handle.closed()
            }
        }.start()
    }

    private fun wifiIp(): String? {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return null
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return null
        return props.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }?.hostAddress
    }

    private fun short(fp: String) = Threnody.short(fp)

    companion object {
        /** Must match `threnody_net::discovery::BLE_SERVICE_UUID`. */
        const val BLE_SERVICE = "7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01"
    }

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
