package livekit.org.webrtc

import android.content.Context

/**
 * Stand-in for LiveKit WebRTC's class in builds without calls
 * (`-Pthrenody.calls=false`, the F-Droid build): the native library then has
 * no WebRTC to hand the context to. Only on the classpath when
 * libs/libwebrtc.jar isn't.
 */
object ContextUtils {
    @JvmStatic
    fun initialize(@Suppress("UNUSED_PARAMETER") context: Context) = Unit
}
