package org.threnody.app

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Looper
import java.security.SecureRandom
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Wi-Fi Direct link upgrades (Appendix L). A session already exists
 * (usually over Bluetooth); one side creates a group with an explicit name
 * and passphrase and sends them over that session, the other joins with
 * the same credentials and dials. No Wi-Fi Protected Setup prompt is
 * involved, and the dial pins the peer's fingerprint.
 */
@Suppress("MissingPermission")
object WifiDirect {
    private var manager: WifiP2pManager? = null
    private var channel: WifiP2pManager.Channel? = null
    private val rng = SecureRandom()

    val permission: String =
        if (Build.VERSION.SDK_INT >= 33) Manifest.permission.NEARBY_WIFI_DEVICES
        else Manifest.permission.ACCESS_FINE_LOCATION

    fun permitted(ctx: Context) = ctx.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    @Synchronized
    private fun init(ctx: Context): Pair<WifiP2pManager, WifiP2pManager.Channel>? {
        val m = manager ?: ctx.getSystemService(WifiP2pManager::class.java) ?: return null
        val c = channel ?: m.initialize(ctx.applicationContext, Looper.getMainLooper(), null) ?: return null
        manager = m
        channel = c
        return m to c
    }

    private fun listener(what: String, ok: () -> Unit) = object : WifiP2pManager.ActionListener {
        override fun onSuccess() = ok()
        override fun onFailure(reason: Int) = Threnody.say("! Wi-Fi Direct $what failed (${reason(reason)})")
    }

    private fun reason(r: Int) = when (r) {
        WifiP2pManager.P2P_UNSUPPORTED -> "unsupported"
        WifiP2pManager.BUSY -> "busy"
        WifiP2pManager.ERROR -> "error"
        else -> "code $r"
    }

    /** Calls `done` with the group's connection info once it has formed (up to ~15 s). */
    private fun whenFormed(m: WifiP2pManager, c: WifiP2pManager.Channel, tries: Int = 30, done: (WifiP2pInfo) -> Unit) {
        m.requestConnectionInfo(c) { info ->
            when {
                info != null && info.groupFormed && info.groupOwnerAddress != null -> done(info)
                tries > 0 -> android.os.Handler(Looper.getMainLooper())
                    .postDelayed({ whenFormed(m, c, tries - 1, done) }, 500)
                else -> Threnody.say("! Wi-Fi Direct group never formed")
            }
        }
    }

    /** Creates a group and offers it to `peer`. */
    fun host(ctx: Context, node: ThrenodyNode, peer: String) {
        if (!permitted(ctx)) return Threnody.say("! Wi-Fi Direct needs the nearby-devices permission")
        val (m, c) = init(ctx) ?: return Threnody.say("! no Wi-Fi Direct on this device")
        val ssid = "DIRECT-th-" + token(4)
        val passphrase = token(20)
        val config = WifiP2pConfig.Builder()
            .setNetworkName(ssid)
            .setPassphrase(passphrase)
            .build()
        val port = Threnody.listenAddr.substringAfterLast(':')
        // A leftover group would make createGroup fail as busy.
        m.removeGroup(c, null)
        m.createGroup(c, config, listener("create group") {
            whenFormed(m, c) { info ->
                val addr = "${info.groupOwnerAddress.hostAddress}:$port"
                Threnody.say("* Wi-Fi Direct group $ssid up at $addr; offering it to ${Threnody.short(peer)}")
                Thread {
                    try { node.offerWifiDirect(peer, ssid, passphrase, addr) }
                    catch (e: Exception) { Threnody.say("! Wi-Fi Direct offer: ${e.message}") }
                }.start()
            }
        })
    }

    /** Joins a group `peer` offered and opens a session over it. */
    fun join(ctx: Context, node: ThrenodyNode, peer: String, ssid: String, passphrase: String, addr: String) {
        if (!permitted(ctx)) return Threnody.say("! Wi-Fi Direct needs the nearby-devices permission")
        val (m, c) = init(ctx) ?: return Threnody.say("! no Wi-Fi Direct on this device")
        Threnody.say("* joining ${Threnody.short(peer)}'s Wi-Fi Direct group $ssid…")
        val config = WifiP2pConfig.Builder()
            .setNetworkName(ssid)
            .setPassphrase(passphrase)
            .build()
        m.connect(c, config, listener("join") {
            whenFormed(m, c) {
                Thread {
                    try {
                        val compact = peer.replace("-", "")
                        node.connect("threnody://$compact@$addr")
                    } catch (e: Exception) {
                        Threnody.say("! Wi-Fi Direct connect: ${e.message}")
                    }
                }.start()
            }
        })
    }

    /** Leaves (or dissolves) our Wi-Fi Direct group. */
    fun leave(ctx: Context) {
        val (m, c) = init(ctx) ?: return
        m.removeGroup(c, listener("leave") { Threnody.say("* Wi-Fi Direct group closed") })
    }

    private fun token(n: Int): String {
        val alphabet = "abcdefghijkmnpqrstuvwxyz23456789"
        return (1..n).map { alphabet[rng.nextInt(alphabet.length)] }.joinToString("")
    }
}
