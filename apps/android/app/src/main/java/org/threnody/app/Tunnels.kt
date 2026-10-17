package org.threnody.app

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.VpnService
import android.system.OsConstants
import android.widget.Toast
import uniffi.threnody_ffi.ThrenodyNode
import java.net.InetAddress

/**
 * WireGuard tunnels to mutually approved contacts (Appendix D): each
 * contact's overlay address reaches the device itself, for apps of the
 * user's choosing (ssh, file sharing). Android has no kernel WireGuard
 * for apps, so the node runs userspace WireGuard on a TUN interface from
 * [ThrenodyVpn]. Only the overlay network goes through it; all other
 * traffic is untouched.
 */
object Tunnels {
    /** The UDP port WireGuard listens on, as on the desktop. */
    const val PORT: UShort = 51820u

    /** Whether a VPN is up that isn't ours (another app's, such as Proton VPN). */
    private fun otherVpn(ctx: Context): Boolean {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return false
        @Suppress("DEPRECATION")
        return cm.allNetworks.any { n ->
            cm.getNetworkCapabilities(n)?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true
        } && Threnody.running()?.tunnelDeviceRunning() != true
    }

    /**
     * Turns tunnels on, asking Android's VPN consent first if needed.
     * Android runs one VPN at a time, so when another is up the user
     * hears what turning tunnels on does to it first.
     */
    fun enable(a: Activity, node: ThrenodyNode) {
        if (!otherVpn(a)) return consentThenOn(a, node)
        SecureBuilder(a)
            .setTitle("Another VPN is on")
            .setMessage("Android runs one VPN at a time, so tunnels would disconnect the VPN you use now. " +
                "If that VPN blocks connections without it (a kill switch, or “Always-on VPN” in " +
                "Settings), this phone has no connection at all until you turn tunnels off again.")
            .setPositiveButton("Turn on anyway") { _, _ -> consentThenOn(a, node) }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun consentThenOn(a: Activity, node: ThrenodyNode) {
        val consent = VpnService.prepare(a)
        if (consent == null) {
            on(a, node)
            return
        }
        ActivityResultRegistry.get(a).launch(consent) { code, _ ->
            if (code == Activity.RESULT_OK) on(a, node)
            else Toast.makeText(a, "Tunnels need permission to set up a VPN", Toast.LENGTH_LONG).show()
        }
    }

    private fun on(a: Activity, node: ThrenodyNode) {
        Privacy.setTunnels(a, true)
        start(a, node)
        Toast.makeText(a, "Tunnels on: mutually approved contacts who turn them on too get one, " +
            "at their overlay address", Toast.LENGTH_LONG).show()
    }

    fun disable(ctx: Context, node: ThrenodyNode) {
        Privacy.setTunnels(ctx, false)
        ctx.startService(Intent(ctx, ThrenodyVpn::class.java).setAction(ThrenodyVpn.ACTION_STOP))
        node.tunnelStopDevice()
        node.setTunnelPort(null)
        Toast.makeText(ctx, "Tunnels off", Toast.LENGTH_SHORT).show()
    }

    /** At startup: brings tunnels back if they were on and consent still holds. */
    fun resume(ctx: Context, node: ThrenodyNode) {
        if (!Privacy.tunnels(ctx)) return
        if (VpnService.prepare(ctx) != null) {
            // Consent was taken away (another VPN app, or the user).
            Privacy.setTunnels(ctx, false)
            Threnody.say("* tunnels off: VPN permission was withdrawn")
            return
        }
        start(ctx, node)
    }

    private fun start(ctx: Context, node: ThrenodyNode) {
        node.setTunnelPort(PORT)
        try {
            ctx.startService(Intent(ctx, ThrenodyVpn::class.java))
        } catch (e: IllegalStateException) {
            // In the background without the foreground service: next time the app opens.
            Threnody.say("! tunnels: ${e.message}")
        }
    }

    /** Builds the TUN interface for [node]'s overlay network and hands it to the node. */
    fun establish(vpn: VpnService, node: ThrenodyNode): Boolean {
        val addr = InetAddress.getByName(node.wireguardInfo().overlay)
        // The overlay is one /48 shared by every Threnody device.
        val net = addr.address.copyOf().also { for (i in 6 until 16) it[i] = 0 }
        val tun = vpn.Builder()
            .setSession("Threnody tunnels")
            .addAddress(addr, 128)
            .addRoute(InetAddress.getByAddress(net), 48)
            // IPv6's minimum, which WireGuard's overhead never pushes past any path.
            .setMtu(1280)
            // Everything else, IPv4 included, goes around the VPN as usual.
            .allowFamily(OsConstants.AF_INET)
            .setBlocking(false)
            .establish() ?: return false
        node.tunnelStartDevice(tun.detachFd())
        return true
    }
}
