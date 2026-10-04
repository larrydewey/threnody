package org.threnody.app

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
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
 * screen or backgrounding the app must not drop sessions.
 */
object Threnody {
    @Volatile private var instance: ThrenodyNode? = null
    var listenAddr: String = ""
        private set

    @Synchronized
    fun get(home: String): ThrenodyNode = instance ?: ThrenodyNode.open(home, null).also {
        listenAddr = it.listen("0.0.0.0:7450")
        instance = it
    }
}

/** Minimal Threnody client: one node, a log, connect / send / approve. */
class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var node: ThrenodyNode
    private lateinit var log: TextView
    private var bleServer: BluetoothServerSocket? = null
    private var current: String? = null
    @Volatile private var running = true

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
        log = TextView(this).apply {
            movementMethod = ScrollingMovementMethod()
            setTextIsSelectable(true)
            textSize = 13f
        }
        for (v in listOf(header, target, connect, message, send, approve, bluetooth)) {
            root.addView(v, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        root.addView(log, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)

        worker.execute {
            try {
                node = Threnody.get(filesDir.resolve("threnody").path)
                val bound = Threnody.listenAddr
                val ip = wifiIp() ?: "127.0.0.1"
                val port = bound.substringAfterLast(':')
                val invite = node.inviteLink("$ip:$port")
                runOnUiThread {
                    header.text = "device ${node.deviceFingerprint()}\n" +
                        "account ${node.accountFingerprint()}\n" +
                        "invite  $invite"
                }
                say("listening on $bound")
                pollEvents()
            } catch (e: Exception) {
                say("! start failed: ${e.message}")
            }
        }

        connect.setOnClickListener {
            val t = target.text.toString().trim()
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
        if (results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }) startBluetooth()
        else say("! Bluetooth permission denied")
    }

    /**
     * Listens on an L2CAP channel and advertises its PSM under the Threnody
     * service UUID. The channel is "insecure" at the Bluetooth layer on
     * purpose: no pairing is needed, the Threnody handshake authenticates.
     */
    @Suppress("MissingPermission")
    private fun startBluetooth() {
        if (bleServer != null) return say("* Bluetooth already on")
        val adapter = getSystemService(BluetoothManager::class.java)?.adapter
            ?: return say("! no Bluetooth adapter")
        val server = try { adapter.listenUsingInsecureL2capChannel() }
            catch (e: Exception) { return say("! L2CAP listen: ${e.message}") }
        bleServer = server
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
            while (running) {
                val sock = try { server.accept() } catch (e: Exception) { break }
                say("* Bluetooth link from ${sock.remoteDevice.address}")
                attach(sock)
            }
        }.start()
    }

    /** Bridges one Bluetooth socket into the node. */
    private fun attach(sock: BluetoothSocket) {
        val out = sock.outputStream
        val link = object : ByteLink {
            override fun send(data: ByteArray): Boolean =
                try { out.write(data); out.flush(); true } catch (e: Exception) { false }
            override fun disconnect() { try { sock.close() } catch (_: Exception) {} }
        }
        val handle = node.attachLink(link, false, "ble", sock.remoteDevice.address, null)
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

    private fun pollEvents() = Thread {
        while (running) {
            when (val e = node.nextEvent(500u)) {
                null -> {}
                is NodeEvent.Connected -> { current = current ?: e.peer; say("* connected ${short(e.peer)}") }
                is NodeEvent.Disconnected -> say("* ${short(e.peer)} disconnected (${e.reason})")
                is NodeEvent.Message -> { current = current ?: e.peer; say("<${short(e.peer)}> ${e.text}") }
                is NodeEvent.ApprovalChanged -> say("* ${short(e.peer)} approval: mutual=${e.mutual}")
                is NodeEvent.File -> say("* ${short(e.peer)} sent ${e.name} (${e.data.size} bytes)")
                else -> say("· $e")
            }
        }
    }.start()

    private fun wifiIp(): String? {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return null
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return null
        return props.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }?.hostAddress
    }

    private fun short(fp: String) = fp.take(9)

    companion object {
        /** Must match `threnody_net::discovery::BLE_SERVICE_UUID`. */
        const val BLE_SERVICE = "7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01"
    }

    private fun say(line: String) = runOnUiThread { log.append(line + "\n") }

    override fun onDestroy() {
        // Stop polling for this Activity; the node itself keeps running.
        running = false
        super.onDestroy()
    }
}
