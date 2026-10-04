package org.threnody.app

import android.app.Activity
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
        log = TextView(this).apply {
            movementMethod = ScrollingMovementMethod()
            setTextIsSelectable(true)
            textSize = 13f
        }
        for (v in listOf(header, target, connect, message, send, approve)) {
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
        approve.setOnClickListener {
            val peer = current ?: return@setOnClickListener say("! no peer yet")
            worker.execute {
                try { node.setApproval(peer, true); say("* approved ${short(peer)}") }
                catch (e: Exception) { say("! approve: ${e.message}") }
            }
        }
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

    private fun say(line: String) = runOnUiThread { log.append(line + "\n") }

    override fun onDestroy() {
        // Stop polling for this Activity; the node itself keeps running.
        running = false
        super.onDestroy()
    }
}
