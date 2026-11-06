package org.threnody.app

import android.app.Activity
import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.nfc.NfcAdapter
import android.nfc.cardemulation.HostApduService
import android.nfc.tech.IsoDep
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.HapticFeedbackConstants
import android.view.View
import uniffi.threnody_ffi.NfcCard
import uniffi.threnody_ffi.NfcFailure
import uniffi.threnody_ffi.NfcOutcome
import uniffi.threnody_ffi.NfcTransport
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Adding a contact by tapping phones (Appendix T), the card side: the
 * phone showing its invite answers a tapping phone through Host Card
 * Emulation. [NfcInviteService] is disabled unless "My invite" is on
 * screen, so a phone with no invite showing answers like one without
 * Threnody. The protocol itself is all in Rust ([NfcCard]).
 */
object NfcShare {
    /** Must match `threnody_core::tap::AID` (and res/xml/nfc_invite.xml). */
    const val AID_HEX = "F0746872656E6F6479"

    private val main = Handler(Looper.getMainLooper())
    @Volatile var card: NfcCard? = null
        private set
    @Volatile private var listener: ((String) -> Unit)? = null

    /** The phone can emulate a card. */
    fun hce(ctx: Context) = NfcAdapter.getDefaultAdapter(ctx) != null &&
        ctx.packageManager.hasSystemFeature(PackageManager.FEATURE_NFC_HOST_CARD_EMULATION)

    /** NFC is switched on. */
    fun on(ctx: Context) = NfcAdapter.getDefaultAdapter(ctx)?.isEnabled == true

    private fun component(ctx: Context) = ComponentName(ctx, NfcInviteService::class.java)

    /**
     * Serves [link] to the next phone that taps this one; [onTapped] gets
     * its fingerprint, once, on the main thread. False if it can't.
     */
    fun arm(ctx: Context, link: String, onTapped: (String) -> Unit): Boolean {
        val c = try { NfcCard(link) } catch (_: Exception) { return false }
        listener = onTapped
        card = c
        val pm = ctx.packageManager
        if (pm.getComponentEnabledSetting(component(ctx)) != PackageManager.COMPONENT_ENABLED_STATE_ENABLED) {
            pm.setComponentEnabledSetting(component(ctx), PackageManager.COMPONENT_ENABLED_STATE_ENABLED, PackageManager.DONT_KILL_APP)
        }
        return true
    }

    /** Nothing is served: the service is off until the next [arm]. */
    fun disarm(ctx: Context) {
        card = null
        listener = null
        val pm = ctx.packageManager
        if (pm.getComponentEnabledSetting(component(ctx)) == PackageManager.COMPONENT_ENABLED_STATE_ENABLED) {
            pm.setComponentEnabledSetting(component(ctx), PackageManager.COMPONENT_ENABLED_STATE_DISABLED, PackageManager.DONT_KILL_APP)
        }
    }

    /** [c] finished an exchange (called from the service, main thread). */
    internal fun finished(ctx: Context, c: NfcCard) {
        if (c !== card) return
        val fp = c.peerFingerprint() ?: return
        card = null
        val l = listener
        listener = null
        main.post { l?.invoke(fp) }
        // Not at once: the service may still be sending its last 9000.
        // The card answers nothing more meanwhile (one exchange each).
        val app = ctx.applicationContext
        main.postDelayed({ if (card == null) disarm(app) }, 1000)
    }
}

/**
 * Feeds the command APDUs of a tapping phone to [NfcShare.card]. Never
 * logs what passes through: links and fingerprints stay in memory.
 */
class NfcInviteService : HostApduService() {
    override fun processCommandApdu(apdu: ByteArray, extras: Bundle?): ByteArray {
        // Nothing armed (say, a fresh process): "no such application".
        val c = NfcShare.card ?: return byteArrayOf(0x6A, 0x82.toByte())
        val r = c.process(apdu)
        if (c.peerFingerprint() != null) NfcShare.finished(this, c)
        return r
    }

    override fun onDeactivated(reason: Int) {}
}

/** The reader side: the phone choosing Scan listens for a tap too. */
object NfcTap {
    /** The phone has NFC. */
    fun available(ctx: Context) = NfcAdapter.getDefaultAdapter(ctx) != null

    /** NFC is switched on. */
    fun enabled(ctx: Context) = NfcAdapter.getDefaultAdapter(ctx)?.isEnabled == true

    /**
     * Reads the next phone showing its invite, with [node]'s fingerprint
     * as all we give it. [onOutcome] runs on a binder thread.
     */
    fun start(activity: Activity, node: ThrenodyNode, onOutcome: (NfcOutcome) -> Unit) {
        val adapter = NfcAdapter.getDefaultAdapter(activity) ?: return
        val flags = NfcAdapter.FLAG_READER_NFC_A or NfcAdapter.FLAG_READER_NFC_B or
            NfcAdapter.FLAG_READER_SKIP_NDEF_CHECK or NfcAdapter.FLAG_READER_NO_PLATFORM_SOUNDS
        adapter.enableReaderMode(activity, { tag ->
            val iso = IsoDep.get(tag)
            if (iso == null) {
                onOutcome(NfcOutcome.Failed(NfcFailure.NOT_AN_INVITE))
                return@enableReaderMode
            }
            // IsoDep throws more than IOException (SecurityException for a
            // stale Tag, IllegalStateException): any of them is the phone gone.
            val outcome = try {
                iso.connect()
                iso.timeout = 3000
                // A fresh reader (and one-time key) for every tag.
                node.nfcReader().run(object : NfcTransport {
                    override fun transceive(apdu: ByteArray): ByteArray? =
                        try { iso.transceive(apdu) } catch (_: Exception) { null }
                })
            } catch (_: Exception) {
                NfcOutcome.Failed(NfcFailure.MOVED)
            } finally {
                try { iso.close() } catch (_: Exception) {}
            }
            onOutcome(outcome)
        }, flags, null)
    }

    fun stop(activity: Activity) {
        try { NfcAdapter.getDefaultAdapter(activity)?.disableReaderMode(activity) } catch (_: IllegalStateException) {}
    }

    /** What to tell the user when a tap failed. */
    fun text(why: NfcFailure) = when (why) {
        NfcFailure.MOVED -> "Hold the phones together until it buzzes"
        NfcFailure.NOT_AN_INVITE -> "That isn't a phone showing a Threnody invite."
        NfcFailure.UNSUPPORTED -> "Their Threnody is too new for this one: update, or scan their QR code."
        NfcFailure.GARBLED -> "Something went wrong: try again, or scan their QR code."
    }
}

/** The buzz both phones give when a tap went through. */
fun tick(view: View) {
    view.performHapticFeedback(
        if (Build.VERSION.SDK_INT >= 30) HapticFeedbackConstants.CONFIRM else HapticFeedbackConstants.LONG_PRESS,
    )
}
