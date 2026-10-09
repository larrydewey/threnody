package org.threnody.app

import android.content.Context
import android.graphics.Bitmap
import android.media.AudioAttributes
import android.media.MediaMetadataRetriever
import android.media.MediaPlayer
import android.media.MediaRecorder
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.LruCache
import java.io.File

/**
 * Voice and video messages: where recordings go, recording sound, and
 * playing one message at a time.
 *
 * Recordings are written straight into the identity's private media
 * folder, beside received photos, and sent from there; a cancelled one is
 * deleted. Voice is AAC in MPEG-4 (.m4a), video H.264 and AAC (.mp4),
 * which the Linux app plays too. Its own recordings (Opus in Ogg, VP8 in
 * WebM) play here as well.
 */
object Clips {
    /** Longest voice message: about 2.4 MB at 32 kb/s. */
    const val MAX_VOICE_MS = 10 * 60_000
    /** Longest video message, so it fits in one file. */
    const val MAX_VIDEO_MS = 60_000
    /** Shorter presses are taken as a tap, not a recording. */
    const val MIN_MS = 700L

    /** A new file to record into. */
    fun file(ctx: Context, persona: String?, video: Boolean): File {
        val dir = Media.dir(ctx, persona).apply { mkdirs() }
        val now = System.currentTimeMillis()
        return File(dir, if (video) "$now-video.mp4" else "$now-voice.m4a")
    }

    fun recorder(ctx: Context): MediaRecorder =
        if (android.os.Build.VERSION.SDK_INT >= 31) MediaRecorder(ctx) else @Suppress("DEPRECATION") MediaRecorder()

    /** `m:ss`. */
    fun clock(ms: Long): String {
        val s = ms / 1000
        return "%d:%02d".format(s / 60, s % 60)
    }

    /** A still from a video message for its bubble, decoded in the background. */
    private val stills = object : LruCache<String, Bitmap>(8 * 1024 * 1024) {
        override fun sizeOf(key: String, value: Bitmap) = value.allocationByteCount
    }

    fun still(ctx: Context, location: String, side: Int, done: (Bitmap?) -> Unit) {
        stills.get(location)?.let { return done(it) }
        val app = ctx.applicationContext
        Threading.io {
            val b = try {
                MediaMetadataRetriever().use { r ->
                    r.setDataSource(app, Uri.parse(location))
                    r.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, side, side)
                }
            } catch (_: Exception) {
                null
            }
            if (b != null) stills.put(location, b)
            done(b)
        }
    }
}

/** Records the microphone into `file` as AAC; [stop] keeps it, [cancel] deletes it. */
class VoiceRecorder(ctx: Context, private val file: File, onLimit: () -> Unit) {
    private val recorder = Clips.recorder(ctx)
    private val started: Long

    init {
        recorder.apply {
            setAudioSource(MediaRecorder.AudioSource.MIC)
            setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            setAudioChannels(1)
            setAudioSamplingRate(44_100)
            setAudioEncodingBitRate(32_000)
            setMaxDuration(Clips.MAX_VOICE_MS)
            setOutputFile(file)
            setOnInfoListener { _, what, _ ->
                if (what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_DURATION_REACHED) onLimit()
            }
            prepare()
            start()
        }
        started = System.currentTimeMillis()
    }

    val elapsed get() = System.currentTimeMillis() - started

    /** Stops; the file and how long it plays, or null if it was too short to keep. */
    fun stop(): Pair<File, Long>? {
        val ms = elapsed.coerceAtMost(Clips.MAX_VOICE_MS.toLong())
        val ok = try {
            recorder.stop()
            true
        } catch (_: RuntimeException) {
            // Stopped before any sound was recorded.
            false
        } finally {
            recorder.release()
        }
        if (!ok || ms < Clips.MIN_MS) {
            file.delete()
            return null
        }
        return file to ms
    }

    fun cancel() {
        try { recorder.stop() } catch (_: RuntimeException) {}
        recorder.release()
        file.delete()
    }
}

/**
 * Plays one voice message at a time. Starting another stops the one
 * playing; [listener] hears where it is, for the play button and the bar.
 */
class ClipPlayer(private val ctx: Context) {
    interface Listener {
        /** `location` is at `positionMs` of `durationMs`, playing or paused. */
        fun progress(location: String, positionMs: Int, durationMs: Int, playing: Boolean)
    }

    var listener: Listener? = null
    var current: String? = null
        private set
    private var player: MediaPlayer? = null
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            report()
            if (player?.isPlaying == true) handler.postDelayed(this, 100)
        }
    }

    val playing get() = player?.isPlaying == true

    /** Plays `location` (from where it was paused), or pauses it if playing. */
    fun toggle(location: String) {
        val p = player
        if (p != null && current == location) {
            if (p.isPlaying) p.pause() else p.start()
            handler.post(tick)
            return
        }
        stop()
        val next = try {
            MediaPlayer().apply {
                setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                        .build()
                )
                setDataSource(ctx, Uri.parse(location))
                setOnCompletionListener {
                    seekTo(0)
                    report()
                }
                prepare()
                start()
            }
        } catch (e: Exception) {
            Threnody.say("! playing a voice message: ${e.message}")
            null
        } ?: return
        player = next
        current = location
        handler.post(tick)
    }

    /** Moves to `fraction` (0–1) of `location`, if it is the one loaded. */
    fun seek(location: String, fraction: Float) {
        val p = player ?: return
        if (current != location) return
        p.seekTo((p.duration * fraction).toInt())
        report()
    }

    /** Where `location` is, if loaded: position and length in ms. */
    fun position(location: String): Pair<Int, Int>? {
        val p = player ?: return null
        return if (current == location) p.currentPosition to p.duration else null
    }

    fun stop() {
        handler.removeCallbacks(tick)
        val was = current
        player?.release()
        player = null
        current = null
        if (was != null) listener?.progress(was, 0, 0, false)
    }

    private fun report() {
        val p = player ?: return
        val loc = current ?: return
        listener?.progress(loc, p.currentPosition, p.duration, p.isPlaying)
    }
}
