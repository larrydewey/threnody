package org.threnody.app

import android.content.ComponentName
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.telecom.CallAudioState
import android.telecom.Connection
import android.telecom.ConnectionRequest
import android.telecom.ConnectionService
import android.telecom.DisconnectCause
import android.telecom.PhoneAccount
import android.telecom.PhoneAccountHandle
import android.telecom.TelecomManager
import android.telecom.VideoProfile

/**
 * Threnody calls as the system sees them (a self-managed
 * [ConnectionService]): car audio and headset buttons answer and hang up,
 * the call's audio follows Bluetooth hands-free, and a phone call that
 * comes in meanwhile puts ours on hold (muted) instead of talking over it.
 *
 * The node still runs the call and its media ([Calls]); this only tells
 * Telecom about it and passes Telecom's requests back. The system never
 * learns who the call is with: the address is the call's random id, and
 * the name shown (for a car's screen) is the generic "Threnody call".
 * When Telecom refuses (an older or locked-down system), calls work as
 * before, without it.
 */
class CallConnectionService : ConnectionService() {
    override fun onCreateIncomingConnection(account: PhoneAccountHandle?, request: ConnectionRequest?): Connection =
        CallConnection(applicationContext, incoming = true).also { it.setRinging() }

    override fun onCreateOutgoingConnection(account: PhoneAccountHandle?, request: ConnectionRequest?): Connection =
        CallConnection(applicationContext, incoming = false).also { it.setDialing() }

    override fun onCreateIncomingConnectionFailed(account: PhoneAccountHandle?, request: ConnectionRequest?) {
        // Telecom won't take it (another app's call it can't hold, say): it rings as before.
        Telecom.failed()
        Calls.showRinging(applicationContext)
    }

    override fun onCreateOutgoingConnectionFailed(account: PhoneAccountHandle?, request: ConnectionRequest?) {
        Telecom.failed()
    }
}

/** One call, as Telecom holds it. */
class CallConnection(val ctx: Context, incoming: Boolean) : Connection() {
    init {
        connectionProperties = PROPERTY_SELF_MANAGED
        connectionCapabilities = CAPABILITY_MUTE or CAPABILITY_SUPPORT_HOLD or CAPABILITY_HOLD
        audioModeIsVoip = true
        setAddress(Uri.fromParts(Telecom.SCHEME, "call", null), TelecomManager.PRESENTATION_ALLOWED)
        setCallerDisplayName("Threnody call", TelecomManager.PRESENTATION_ALLOWED)
        setVideoState(if (Calls.current?.video == true) VideoProfile.STATE_BIDIRECTIONAL else VideoProfile.STATE_AUDIO_ONLY)
        Telecom.attach(this, incoming)
    }

    /** The system wants an incoming call shown: our ringing notification does that. */
    override fun onShowIncomingCallUi() = Calls.showRinging(ctx)

    /** Answered from a headset, a car or a watch. */
    override fun onAnswer() = Calls.answerFromSystem(ctx, video = false)

    override fun onAnswer(videoState: Int) = Calls.answerFromSystem(ctx, video = VideoProfile.isVideo(videoState))

    override fun onReject() = Calls.hangUp(ctx)

    override fun onDisconnect() = Calls.hangUp(ctx)

    override fun onAbort() = Calls.hangUp(ctx)

    /** Another call (a phone call) took over: ours goes quiet until it's back. */
    override fun onHold() {
        setOnHold()
        Calls.hold(true)
    }

    override fun onUnhold() {
        setActive()
        Calls.hold(false)
    }

    @Deprecated("Replaced by CallEndpoint on API 34; still delivered there")
    override fun onCallAudioStateChanged(state: CallAudioState?) {
        state ?: return
        Calls.systemRoute(state.route == CallAudioState.ROUTE_SPEAKER, state.isMuted)
    }

    /** Ends Telecom's call with `cause` and forgets it. */
    fun end(cause: Int) {
        setDisconnected(DisconnectCause(cause))
        destroy()
    }
}

/**
 * The bridge between [Calls] and Telecom: registers the account, places
 * and announces calls, and keeps the one [CallConnection].
 */
object Telecom {
    const val SCHEME = "threnody"
    private const val ACCOUNT_ID = "threnody-calls"

    @Volatile private var connection: CallConnection? = null
    /** Telecom refused the last call: carry on without it. */
    @Volatile private var refused = false

    private fun handle(ctx: Context) =
        PhoneAccountHandle(ComponentName(ctx, CallConnectionService::class.java), ACCOUNT_ID)

    private fun manager(ctx: Context): TelecomManager? = ctx.getSystemService(TelecomManager::class.java)

    /** Registers Threnody's (self-managed) calling account, once per start. */
    fun register(ctx: Context): Boolean = try {
        val account = PhoneAccount.builder(handle(ctx), "Threnody")
            .setCapabilities(PhoneAccount.CAPABILITY_SELF_MANAGED or PhoneAccount.CAPABILITY_VIDEO_CALLING or
                PhoneAccount.CAPABILITY_SUPPORTS_VIDEO_CALLING)
            .setSupportedUriSchemes(listOf(SCHEME))
            .build()
        manager(ctx)?.registerPhoneAccount(account)
        true
    } catch (e: Exception) {
        Threnody.say("! telecom: ${e.javaClass.simpleName}")
        false
    }

    /** Whether Telecom will take a call now (the permission is granted at install). */
    private fun usable(ctx: Context) = try {
        ctx.checkSelfPermission(android.Manifest.permission.MANAGE_OWN_CALLS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED && register(ctx)
    } catch (_: Exception) { false }

    /** An incoming call: Telecom then asks us to show it ([CallConnection.onShowIncomingCallUi]). */
    fun incoming(ctx: Context, video: Boolean): Boolean {
        if (!usable(ctx)) return false
        refused = false
        return try {
            manager(ctx)?.addNewIncomingCall(handle(ctx), Bundle().apply {
                putInt(TelecomManager.EXTRA_INCOMING_VIDEO_STATE,
                    if (video) VideoProfile.STATE_BIDIRECTIONAL else VideoProfile.STATE_AUDIO_ONLY)
            })
            true
        } catch (e: Exception) {
            Threnody.say("! telecom incoming: ${e.javaClass.simpleName}")
            false
        }
    }

    /** Our call: Telecom routes its audio (and Bluetooth hands-free) while it lasts. */
    fun outgoing(ctx: Context, video: Boolean) {
        if (!usable(ctx)) return
        refused = false
        try {
            manager(ctx)?.placeCall(Uri.fromParts(SCHEME, "call", null), Bundle().apply {
                putParcelable(TelecomManager.EXTRA_PHONE_ACCOUNT_HANDLE, handle(ctx))
                putInt(TelecomManager.EXTRA_START_CALL_WITH_VIDEO_STATE,
                    if (video) VideoProfile.STATE_BIDIRECTIONAL else VideoProfile.STATE_AUDIO_ONLY)
            })
        } catch (e: Exception) {
            Threnody.say("! telecom outgoing: ${e.javaClass.simpleName}")
        }
    }

    fun attach(c: CallConnection, incoming: Boolean) {
        connection?.end(DisconnectCause.OTHER)
        connection = c
        Threnody.say("* telecom: ${if (incoming) "incoming" else "outgoing"} call")
        Calls.handOverAudio(c.ctx)
    }

    fun failed() {
        refused = true
        connection = null
    }

    /** Whether Telecom holds the call (so it, not we, routes the audio). */
    fun active(): Boolean = connection != null && !refused

    /** The call is answered: Telecom moves it to active (audio focus, hands-free). */
    fun answered() {
        connection?.setActive()
    }

    /** The call ended, for whatever reason; Telecom shows it as `cause`. */
    fun ended(cause: Int) {
        connection?.end(cause)
        connection = null
    }

    /** Speaker or not, through Telecom (it knows about cars and headsets). */
    @Suppress("DEPRECATION")
    fun speaker(on: Boolean): Boolean {
        val c = connection ?: return false
        c.setAudioRoute(if (on) CallAudioState.ROUTE_SPEAKER else CallAudioState.ROUTE_WIRED_OR_EARPIECE)
        return true
    }

    /** Video on or off, for the car's or watch's picture of the call. */
    fun video(on: Boolean) {
        connection?.setVideoState(if (on) VideoProfile.STATE_BIDIRECTIONAL else VideoProfile.STATE_AUDIO_ONLY)
    }

    /** API levels where Telecom's self-managed calls behave (Android 10 has them, with quirks). */
    val supported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
}
