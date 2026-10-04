package org.threnody.app

import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.os.Bundle
import android.text.method.ScrollingMovementMethod
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import java.util.concurrent.Executors
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Diagnostics: identities, the node's event log, and manual Bluetooth and
 * Wi-Fi Direct controls for testing transports.
 */
class LogActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private var node: ThrenodyNode? = null
    private lateinit var log: TextView
    private var seen: List<Pair<BluetoothDevice, Int>> = emptyList()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val bar = TopBar(this) { finish() }.apply { title.text = "Diagnostics" }
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), 0, dp(16), 0)
        }
        val header = label("", 12f, R.color.muted).apply { setTextIsSelectable(true); typeface = Typeface.MONOSPACE }
        val target = EditText(this).apply { hint = "ble <n> after a scan"; isSingleLine = true }
        fun button(text: String, onClick: () -> Unit) = Button(this).apply {
            this.text = text
            isAllCaps = false
            setOnClickListener { onClick() }
        }
        val row1 = LinearLayout(this).apply {
            addView(button("Start Bluetooth") {
                if (Bluetooth.permitted(this@LogActivity)) startBluetooth() else requestPermissions(Bluetooth.permissions, 1)
            }, LinearLayout.LayoutParams(0, -2, 1f))
            addView(button("Scan Bluetooth") {
                if (Bluetooth.permitted(this@LogActivity)) scanBluetooth() else requestPermissions(Bluetooth.permissions, 2)
            }, LinearLayout.LayoutParams(0, -2, 1f))
        }
        val row2 = LinearLayout(this).apply {
            addView(button("Dial") { dial(target.text.toString().trim()) }, LinearLayout.LayoutParams(0, -2, 1f))
            addView(button("Leave Wi-Fi Direct") { WifiDirect.leave(applicationContext) }, LinearLayout.LayoutParams(0, -2, 1f))
        }
        log = TextView(this).apply {
            movementMethod = ScrollingMovementMethod()
            setTextIsSelectable(true)
            textSize = 12f
            typeface = Typeface.MONOSPACE
            setTextColor(color(R.color.text))
        }
        for (v in listOf(header, row1, target, row2)) body.addView(v, matchWrap)
        body.addView(log, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        root.addView(bar, matchWrap)
        root.addView(body, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, body)

        worker.execute {
            val n = Threnody.start(this)
            node = n
            runOnUiThread {
                header.text = "device  ${n.deviceFingerprint()}\naccount ${n.accountFingerprint()}\n" +
                    "listen  ${Threnody.listenAddr}"
            }
        }
    }

    private fun dial(t: String) {
        val n = node ?: return
        val i = t.removePrefix("ble").trim().toIntOrNull()
        val found = i?.let { seen.getOrNull(it - 1) } ?: return say("! no such Bluetooth device; scan first")
        worker.execute { Bluetooth.dial(n, found.first, found.second, null) }
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (results.isEmpty() || results.any { it != PackageManager.PERMISSION_GRANTED }) {
            say("! Bluetooth permission denied")
        } else if (code == 2) scanBluetooth() else startBluetooth()
    }

    private fun startBluetooth() {
        val n = node ?: return
        if (Bluetooth.running) return say("* Bluetooth already on")
        worker.execute { Bluetooth.start(applicationContext, n) }
    }

    private fun scanBluetooth() {
        val n = node ?: return
        say("* scanning Bluetooth for 8s…")
        Bluetooth.scanOnce(this, n) { found ->
            seen = found
            if (found.isEmpty()) say("* no Threnody devices nearby")
            found.forEachIndexed { i, (d, psm) -> say("  ${i + 1}: ${d.address} psm $psm — dial with \"ble ${i + 1}\"") }
        }
    }

    private fun say(line: String) = Threnody.say(line)

    override fun onStart() {
        super.onStart()
        Threnody.visible++
        log.text = Threnody.watchLog { line -> runOnUiThread { log.append(line + "\n") } }
    }

    override fun onStop() {
        Threnody.visible--
        Threnody.watchLog(null)
        super.onStop()
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }
}
