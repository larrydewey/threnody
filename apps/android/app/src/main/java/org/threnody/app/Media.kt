package org.threnody.app

import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.util.LruCache
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.concurrent.Executors

/**
 * Pictures: which files are images, where received ones are kept, and
 * decoding them safely for display.
 *
 * Received images stay in the app's private storage rather than in shared
 * Downloads, so gallery apps and other apps with media access don't see
 * them (a sensitive photo least of all). The viewer can save a copy.
 */
object Media {
    private val IMAGE = setOf("jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "bmp")
    /** Formats whose metadata the node strips; others are converted to JPEG. */
    private val STRIPPABLE = setOf("jpg", "jpeg", "png", "webp")
    /** Larger images are refused rather than decoded (decompression bombs). */
    private const val MAX_PIXELS = 100_000_000L

    private val decoder = Executors.newFixedThreadPool(2)
    private val thumbs = object : LruCache<String, Bitmap>(24 * 1024 * 1024) {
        override fun sizeOf(key: String, value: Bitmap) = value.allocationByteCount
    }

    fun isImage(name: String) = name.substringAfterLast('.', "").lowercase() in IMAGE

    /**
     * Makes a picked file ready to send: an image the node can't strip
     * metadata from (HEIC, AVIF, …) is re-encoded as JPEG, which carries
     * none. Returns the name and bytes to send.
     */
    fun prepare(ctx: Context, name: String, data: ByteArray): Pair<String, ByteArray> {
        val ext = name.substringAfterLast('.', "").lowercase()
        if (!isImage(name) || ext in STRIPPABLE || ext == "gif") return name to data
        val bitmap = decode(ImageDecoder.createSource(java.nio.ByteBuffer.wrap(data)), 4096)
            ?: throw IllegalArgumentException("couldn't read $name")
        val out = ByteArrayOutputStream()
        bitmap.compress(Bitmap.CompressFormat.JPEG, 92, out)
        return name.substringBeforeLast('.') + ".jpg" to out.toByteArray()
    }

    /** The private folder for an identity's files: the main one's, or a persona's. */
    fun dir(ctx: Context, persona: String?): File =
        File(ctx.filesDir, if (persona == null) "media" else "media-p-$persona")

    /** Keeps a file privately; returns its location for history. */
    fun savePrivate(ctx: Context, name: String, data: ByteArray, persona: String? = null): String? = try {
        val dir = dir(ctx, persona).apply { mkdirs() }
        val safe = name.substringAfterLast('/').replace(Regex("[^A-Za-z0-9._-]"), "_").takeLast(80)
        val f = File(dir, "${System.currentTimeMillis()}-${(1000..9999).random()}-$safe")
        f.writeBytes(data)
        Uri.fromFile(f).toString()
    } catch (e: Exception) {
        Threnody.say("! saving $name: ${e.message}")
        null
    }

    /**
     * Decodes at most `maxSide` pixels on the longer side, refusing
     * oversized or unreadable images. Never throws.
     */
    fun decode(source: ImageDecoder.Source, maxSide: Int): Bitmap? = try {
        ImageDecoder.decodeBitmap(source) { d, info, _ ->
            val (w, h) = info.size.width to info.size.height
            if (w <= 0 || h <= 0 || w.toLong() * h > MAX_PIXELS) throw IllegalArgumentException("too large")
            val scale = maxOf(1.0, maxOf(w, h).toDouble() / maxSide)
            d.setTargetSize((w / scale).toInt().coerceAtLeast(1), (h / scale).toInt().coerceAtLeast(1))
            d.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        }
    } catch (_: Throwable) {
        null
    }

    fun source(ctx: Context, location: String): ImageDecoder.Source {
        val uri = Uri.parse(location)
        return if (uri.scheme == "file") ImageDecoder.createSource(File(uri.path ?: ""))
        else ImageDecoder.createSource(ctx.contentResolver, uri)
    }

    fun isGif(name: String) = name.substringAfterLast('.', "").lowercase() == "gif"

    /**
     * An animated GIF at `location`, at most `maxSide` pixels on the longer
     * side, decoded in the background; `done` gets null if it isn't one.
     */
    fun animated(ctx: Context, location: String, maxSide: Int, done: (android.graphics.drawable.Drawable?) -> Unit) {
        val app = ctx.applicationContext
        animated({ source(app, location) }, maxSide, done)
    }

    /** As above, from bytes or wherever `src` reads. */
    fun animated(src: () -> ImageDecoder.Source, maxSide: Int, done: (android.graphics.drawable.Drawable?) -> Unit) {
        decoder.execute {
            val d = try {
                ImageDecoder.decodeDrawable(src()) { dec, info, _ ->
                    val (w, h) = info.size.width to info.size.height
                    if (w <= 0 || h <= 0 || w.toLong() * h > MAX_PIXELS) throw IllegalArgumentException("too large")
                    val scale = maxOf(1.0, maxOf(w, h).toDouble() / maxSide)
                    dec.setTargetSize((w / scale).toInt().coerceAtLeast(1), (h / scale).toInt().coerceAtLeast(1))
                }
            } catch (_: Throwable) {
                null
            }
            done(d as? android.graphics.drawable.AnimatedImageDrawable)
        }
    }

    /** A thumbnail for `location`, from the cache or decoded in the background. */
    fun thumbnail(ctx: Context, location: String, side: Int, done: (Bitmap?) -> Unit) {
        val key = "$location@$side"
        thumbs.get(key)?.let { return done(it) }
        val app = ctx.applicationContext
        decoder.execute {
            val b = decode(source(app, location), side)
            if (b != null) thumbs.put(key, b)
            done(b)
        }
    }

    /**
     * Deletes privately kept images no message refers to any more: deleted
     * ones, and ones whose messages disappeared. Recent files are left, as
     * their message may not be recorded yet.
     */
    fun sweep(ctx: Context, node: uniffi.threnody_ffi.ThrenodyNode, persona: String? = null) {
        val dir = dir(ctx, persona)
        val files = dir.listFiles() ?: return
        val used = try {
            val chats = Threnody.conversations(node, persona).flatMap { node.history(it.device, 10_000u) }
            val groups = node.groups().flatMap { node.groupHistory(it.id, 10_000u) }
            (chats + groups).mapNotNull { it.file?.location }.toSet()
        } catch (_: Exception) {
            return
        }
        val old = System.currentTimeMillis() - 10 * 60_000
        for (f in files) {
            if (f.lastModified() < old && Uri.fromFile(f).toString() !in used) f.delete()
        }
    }

    /** Removes a privately kept image (when its message is deleted). */
    fun forget(ctx: Context, location: String?) {
        val uri = location?.let(Uri::parse) ?: return
        val path = uri.path ?: return
        if (uri.scheme == "file" && path.startsWith(ctx.filesDir.path + "/media")) File(path).delete()
    }
}
