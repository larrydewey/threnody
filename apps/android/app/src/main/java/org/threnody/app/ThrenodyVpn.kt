package org.threnody.app

import android.content.Intent
import android.net.VpnService

/**
 * The VPN slot for WireGuard tunnels ([Tunnels]): a TUN interface routing
 * only the overlay network, carried by the node's userspace WireGuard.
 */
class ThrenodyVpn : VpnService() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stop()
            stopSelf()
            return START_NOT_STICKY
        }
        Threading.background {
            try {
                val node = Threnody.start(applicationContext)
                if (node.tunnelPort() == null) node.setTunnelPort(Tunnels.PORT)
                if (Tunnels.establish(this, node)) Threnody.say("* tunnels: overlay network up")
                else Threnody.say("! tunnels: no VPN permission")
            } catch (e: Exception) {
                Threnody.say("! tunnels: ${e.message}")
            }
        }
        return START_STICKY
    }

    /** Another VPN took the slot, or the user switched it off in Settings. */
    override fun onRevoke() {
        Privacy.setTunnels(this, false)
        Threnody.running()?.setTunnelPort(null)
        stop()
        super.onRevoke()
    }

    override fun onDestroy() {
        stop()
        super.onDestroy()
    }

    private fun stop() {
        Threnody.running()?.tunnelStopDevice()
    }

    companion object {
        const val ACTION_STOP = "org.threnody.app.STOP_TUNNELS"
    }
}
