package org.threnody.app

import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.util.LruCache
import java.io.ByteArrayOutputStream
import java.io.File
import uniffi.threnody_ffi.ThrenodyNode
import uniffi.threnody_ffi.imageNeedsConversion
import uniffi.threnody_ffi.isSealedMedia
import uniffi.threnody_ffi.mediaSize
import uniffi.threnody_ffi.secureDeleteFile

/**
 * Pictures: which files are images, where received ones are kept, and
 * decoding them safely for display.
 *
 * Received images stay in the app's private storage rather than in shared
 * Downloads, so gallery apps and other apps with media access don't see
 * them (a sensitive photo least of all). The viewer can save a copy.
 *
 * Everything kept there (photos, voice and video messages, a persona's
 * files) is sealed under the main identity (`sealMedia`), so a copy of the
 * app's files is useless off the device, as the identity is. Pictures
 * decode from bytes decrypted in memory, and clips play from them
 * ([BytesSource]); a video, or a file opened in another app, gets a
 * decrypted copy in the cache ([plainCopy]), deleted when it's done with,
 * on lock and at start. Files kept before sealing are read as they are
 * until [sealAll] seals them at start.
 */
object Media {
    private val IMAGE = setOf("jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "bmp")
    /** Larger images are refused rather than decoded (decompression bombs). */
    private const val MAX_PIXELS = 100_000_000L

    private val decoder: java.util.concurrent.ExecutorService = Threading.io
    private val thumbs = object : LruCache<String, Bitmap>(24 * 1024 * 1024) {
        override fun sizeOf(key: String, value: Bitmap) = value.allocationByteCount
    }

    fun isImage(name: String) = name.substringAfterLast('.', "").lowercase() in IMAGE

    private val VIDEO = setOf("mp4", "m4v", "mov", "3gp", "webm", "mkv")

    fun isVideo(name: String) = name.substringAfterLast('.', "").lowercase() in VIDEO

    /**
     * A small JPEG of the photo or video at `location`, for an offered
     * file's bubble before it is downloaded; null if it isn't one or
     * can't be made small enough. Encoded afresh from pixels, so nothing
     * of the original's metadata goes with it (the node strips it again
     * all the same). Reads the file: call off the main thread.
     */
    fun offerThumbnail(ctx: Context, name: String, location: String): ByteArray? {
        val side = 320
        val bitmap = try {
            when {
                isImage(name) -> decode(source(ctx, location), side)
                isVideo(name) -> android.media.MediaMetadataRetriever().use { r ->
                    val sealed = dataSource(location)
                    if (sealed != null) r.setDataSource(sealed) else r.setDataSource(ctx, Uri.parse(location))
                    r.getScaledFrameAtTime(0, android.media.MediaMetadataRetriever.OPTION_CLOSEST_SYNC, side, side)
                }
                else -> null
            }
        } catch (_: Throwable) {
            null
        } ?: return null
        val limit = uniffi.threnody_ffi.maxOfferThumbnail().toInt()
        for (quality in listOf(70, 55, 40, 30)) {
            val out = ByteArrayOutputStream()
            bitmap.compress(Bitmap.CompressFormat.JPEG, quality, out)
            if (out.size() <= limit) return out.toByteArray()
        }
        return null
    }

    /**
     * Makes a picked file ready to send: an image the node can't strip
     * metadata from (HEIC, AVIF, DNG, …) is re-encoded as JPEG, which
     * carries none. Its content decides before its name, so a HEIC called
     * "file" or "photo.jpg" is converted too, and a video called
     * "clip.heic" isn't. Returns the name and bytes to send.
     */
    fun prepare(ctx: Context, name: String, data: ByteArray): Pair<String, ByteArray> {
        if (!imageNeedsConversion(name, data.copyOf(minOf(data.size, 32)))) return name to data
        return converted(name, ImageDecoder.createSource(java.nio.ByteBuffer.wrap(data)))
    }

    /**
     * A picked file too large for one message, unread: whether it is a
     * photo to convert first (see [prepare]), from its first bytes.
     */
    fun needsConverting(ctx: Context, name: String, uri: Uri): Boolean {
        val head = ctx.contentResolver.openInputStream(uri)?.use { s ->
            val b = ByteArray(32)
            var n = 0
            while (n < b.size) {
                val r = s.read(b, n, b.size - n)
                if (r < 0) break
                n += r
            }
            b.copyOf(n)
        } ?: return false
        return imageNeedsConversion(name, head)
    }

    /** The photo `src` reads as a JPEG of at most 4096 px a side, which carries no metadata. */
    fun converted(name: String, src: ImageDecoder.Source): Pair<String, ByteArray> {
        val bitmap = decode(src, 4096) ?: throw IllegalArgumentException("couldn't read $name")
        val out = ByteArrayOutputStream()
        bitmap.compress(Bitmap.CompressFormat.JPEG, 92, out)
        return name.substringBeforeLast('.') + ".jpg" to out.toByteArray()
    }

    /** The private folder for an identity's files: the main one's, or a persona's. */
    fun dir(ctx: Context, persona: String?): File =
        File(ctx.filesDir, if (persona == null) "media" else "media-p-$persona")

    /** The main node, whose identity seals kept files. */
    private fun sealer(): ThrenodyNode = Threnody.running() ?: throw IllegalStateException("Threnody isn't running")

    /** A new file's place in an identity's private folder, for a file called `name`. */
    private fun newPrivate(ctx: Context, name: String, persona: String?): File {
        val dir = dir(ctx, persona).apply { mkdirs() }
        val safe = name.substringAfterLast('/').replace(Regex("[^A-Za-z0-9._-]"), "_").takeLast(80)
        return File(dir, "${System.currentTimeMillis()}-${(1000..9999).random()}-$safe")
    }

    /**
     * An empty private file for a download of `name` (the main identity's),
     * which the node seals as it writes; null if none can be made.
     */
    fun reserve(ctx: Context, name: String): uniffi.threnody_ffi.ReservedFile? = if (Threnody.wiping) {
        null
    } else try {
        val f = newPrivate(ctx, name, null)
        f.createNewFile()
        uniffi.threnody_ffi.ReservedFile(f.path, Uri.fromFile(f).toString())
    } catch (e: Exception) {
        Threnody.say("! reserving $name: ${e.message}")
        null
    }

    /** Keeps a file privately, sealed; returns its location for history. */
    fun savePrivate(ctx: Context, name: String, data: ByteArray, persona: String? = null): String? = if (Threnody.wiping) {
        null
    } else try {
        val f = newPrivate(ctx, name, persona)
        sealer().sealMedia(data, f.path)
        Uri.fromFile(f).toString()
    } catch (e: Exception) {
        Threnody.say("! saving $name: ${e.message}")
        null
    }

    /** The path of a sealed file kept privately at `location`, or null for anything else. */
    private fun sealedPath(location: String): String? {
        val path = if (location.startsWith("file:")) Uri.parse(location).path else if (location.startsWith("/")) location else null
        return path?.takeIf { isSealedMedia(it) }
    }

    /** What the sealed file at `location` holds, decrypted in memory; null if it isn't sealed. Throws if it can't be opened. */
    fun opened(location: String): ByteArray? = sealedPath(location)?.let { sealer().openMedia(it) }

    /**
     * A clip at `location` decrypted in memory for MediaPlayer or
     * MediaMetadataRetriever, so no plaintext file is written; null if it
     * isn't sealed (play it from its location).
     */
    fun dataSource(location: String): android.media.MediaDataSource? {
        val path = sealedPath(location) ?: return null
        val size = mediaSize(path).toLong()
        return if (size > IN_MEMORY) SealedSource(sealer(), path, size) else BytesSource(sealer().openMedia(path))
    }

    /** Larger sealed files are read a piece at a time, never decrypted whole. */
    const val IN_MEMORY = 16L * 1024 * 1024

    /** The path and plaintext size of the sealed file at `location`, or null for anything else. */
    fun sealedSize(location: String): Pair<String, Long>? =
        sealedPath(location)?.let { it to mediaSize(it).toLong() }

    /** Up to `len` bytes of the sealed file at `path` from `offset`, decrypting only those. */
    fun readSealed(path: String, offset: Long, len: Int): ByteArray =
        sealer().readMediaRange(path, offset.toULong(), len.toUInt())

    /**
     * How many bytes the file at `location` holds as read (a sealed one
     * opened); null if it is gone.
     */
    fun size(ctx: Context, location: String): Long? = try {
        val sealed = sealedPath(location)
        when {
            sealed != null -> mediaSize(sealed).toLong()
            location.startsWith("content:") || location.startsWith("file:") -> {
                val uri = Uri.parse(location)
                ctx.contentResolver.openFileDescriptor(uri, "r")?.use { it.statSize }?.takeIf { it >= 0 }
                    ?: ctx.contentResolver.query(uri, arrayOf(android.provider.OpenableColumns.SIZE), null, null, null)?.use { c ->
                        if (c.moveToFirst() && !c.isNull(0)) c.getLong(0) else null
                    }
            }
            else -> File(location).takeIf { it.isFile }?.length()
        }
    } catch (_: Exception) {
        null
    }

    /**
     * Up to `len` bytes of the file at `location` from `offset`, fewer
     * only at its end; a sealed file is decrypted only where they lie.
     * Null if it can't be read.
     */
    fun readAt(ctx: Context, location: String, offset: Long, len: Int): ByteArray? = try {
        val sealed = sealedPath(location)
        if (sealed != null) {
            sealer().readMediaRange(sealed, offset.toULong(), len.toUInt())
        } else {
            val pfd = if (location.startsWith("content:") || location.startsWith("file:")) {
                ctx.contentResolver.openFileDescriptor(Uri.parse(location), "r")
            } else {
                android.os.ParcelFileDescriptor.open(File(location), android.os.ParcelFileDescriptor.MODE_READ_ONLY)
            }
            pfd?.use { fd ->
                java.io.FileInputStream(fd.fileDescriptor).channel.use { ch ->
                    val buf = java.nio.ByteBuffer.allocate(len)
                    var at = offset
                    while (buf.hasRemaining()) {
                        val n = ch.read(buf, at)
                        if (n < 0) break
                        at += n
                    }
                    buf.array().copyOf(buf.position())
                }
            }
        }
    } catch (_: Exception) {
        null
    }

    /** Where decrypted copies wait: the cache, never backed up. */
    private fun plainDir(ctx: Context) = File(ctx.cacheDir, "plain")

    /**
     * A decrypted copy of the sealed file at `location`, for what can only
     * read a file (VideoView, another app); null if it isn't sealed. Delete
     * it with [dropPlain] when done; [wipePlain] deletes any left.
     */
    fun plainCopy(ctx: Context, location: String): File? {
        val path = sealedPath(location) ?: return null
        val dir = plainDir(ctx).apply { mkdirs() }
        val f = File(dir, "${System.nanoTime()}-${File(path).name}")
        try {
            sealer().openMediaFile(path, f.path)
        } catch (e: Exception) {
            f.delete()
            throw e
        }
        return f
    }

    fun dropPlain(f: File) {
        try { secureDeleteFile(f.path) } catch (_: Exception) { f.delete() }
    }

    /** Deletes every decrypted copy (at start and on lock). */
    fun wipePlain(ctx: Context) {
        plainDir(ctx).listFiles()?.forEach { dropPlain(it) }
    }

    /**
     * Seals, in place, files every identity's folder kept before media was
     * sealed (the plaintext is overwritten). Run before the first sweep.
     */
    fun sealAll(ctx: Context) {
        val dirs = ctx.filesDir.listFiles { f -> f.isDirectory && (f.name == "media" || f.name.startsWith("media-p-")) }
            ?: return
        for (dir in dirs) {
            try {
                val n = sealer().sealMediaDir(dir.path)
                if (n > 0u) Threnody.say("* sealed $n kept files in ${dir.name}")
            } catch (e: Exception) {
                Threnody.say("! sealing kept files: ${e.message}")
            }
        }
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

    /**
     * What to decode the picture at `location` from: a sealed one decrypted
     * in memory (nothing decodes if it can't be opened). Never throws.
     */
    fun source(ctx: Context, location: String): ImageDecoder.Source {
        val sealed = try {
            opened(location)
        } catch (_: Exception) {
            ByteArray(0)
        }
        if (sealed != null) return ImageDecoder.createSource(java.nio.ByteBuffer.wrap(sealed))
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
     * ones, and ones whose messages disappeared. Notes to self count as
     * messages. Recent files are left, as their message may not be recorded
     * yet.
     */
    fun sweep(ctx: Context, node: uniffi.threnody_ffi.ThrenodyNode, persona: String? = null) {
        val dir = dir(ctx, persona)
        val files = dir.listFiles() ?: return
        // Every conversation's files, message requests included; nothing is
        // deleted if some history can't be read.
        val used = try {
            node.fileLocations().toSet()
        } catch (_: Exception) {
            return
        }
        val old = System.currentTimeMillis() - 10 * 60_000
        for (f in files) {
            if (f.lastModified() < old && Uri.fromFile(f).toString() !in used) f.delete()
        }
    }

    /**
     * The bytes at `location` (a `content:` or `file:` URI, or a path), if
     * still there and at most `max` bytes; null otherwise. A sealed file is
     * decrypted.
     */
    fun read(ctx: Context, location: String, max: Long): ByteArray? = try {
        val sealed = sealedPath(location)
        val input = when {
            sealed != null -> null
            location.startsWith("content:") || location.startsWith("file:") -> ctx.contentResolver.openInputStream(Uri.parse(location))
            else -> java.io.FileInputStream(location)
        }
        if (sealed != null) {
            if (mediaSize(sealed).toLong() in 0..max) sealer().openMedia(sealed) else null
        } else input?.use { s ->
            val out = ByteArrayOutputStream()
            val buf = ByteArray(64 * 1024)
            var total = 0L
            while (true) {
                val n = s.read(buf)
                if (n < 0) break
                total += n
                if (total > max) return@use null
                out.write(buf, 0, n)
            }
            out.toByteArray()
        }
    } catch (_: Exception) {
        null
    }

    /** Removes a privately kept image (when its message is deleted). */
    fun forget(ctx: Context, location: String?) {
        val uri = location?.let(Uri::parse) ?: return
        val path = uri.path ?: return
        if (uri.scheme == "file" && path.startsWith(ctx.filesDir.path + "/media")) File(path).delete()
    }
}

/**
 * A sealed clip too large to decrypt in memory, for MediaPlayer and
 * MediaMetadataRetriever: read a window at a time, decrypting only the
 * chunks it lies in.
 */
class SealedSource(
    private val node: ThrenodyNode,
    private val path: String,
    private val size: Long,
) : android.media.MediaDataSource() {
    private var at = -1L
    private var window = ByteArray(0)

    @Synchronized
    override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
        if (position >= this.size) return -1
        if (size == 0) return 0
        if (at < 0 || position < at || position >= at + window.size) {
            window.fill(0)
            at = position - position % WINDOW
            window = try {
                node.readMediaRange(path, at.toULong(), WINDOW.toUInt())
            } catch (_: Exception) {
                at = -1
                ByteArray(0)
            }
            if (window.isEmpty()) return -1
        }
        val from = (position - at).toInt()
        val n = minOf(size, window.size - from)
        System.arraycopy(window, from, buffer, offset, n)
        return n
    }

    override fun getSize(): Long = size

    @Synchronized
    override fun close() {
        window.fill(0)
        window = ByteArray(0)
        at = -1
    }

    private companion object {
        const val WINDOW = 256 * 1024
    }
}

/** A clip decrypted in memory, for MediaPlayer and MediaMetadataRetriever. */
class BytesSource(private val data: ByteArray) : android.media.MediaDataSource() {
    override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
        if (position >= data.size) return -1
        val n = minOf(size.toLong(), data.size - position).toInt()
        System.arraycopy(data, position.toInt(), buffer, offset, n)
        return n
    }

    override fun getSize(): Long = data.size.toLong()

    override fun close() {
        data.fill(0)
    }
}

/**
 * Where the main identity keeps files, for getting them from our other
 * devices and giving them ours (Appendix J). Files that come over are
 * kept privately, never in Downloads.
 */
class AndroidFiles(private val ctx: Context) : uniffi.threnody_ffi.SyncedFileStore {
    override fun read(location: String, max: ULong): ByteArray? =
        Media.read(ctx, location, max.toLong().coerceAtLeast(0))

    override fun keep(name: String, data: ByteArray, clip: Boolean): String? =
        Media.savePrivate(ctx, name, data, null)
}

/**
 * Where the main identity keeps files too large for one message
 * (Appendix U): what it sends is read in pieces where it is (a picked
 * file through its `content:` URI, never read whole), and what it
 * downloads is sealed into the private folder as it is written.
 */
class AndroidLargeFiles(private val ctx: Context) : uniffi.threnody_ffi.LargeFileStore {
    override fun size(location: String): ULong? = Media.size(ctx, location)?.toULong()

    override fun readAt(location: String, offset: ULong, len: UInt): ByteArray? =
        Media.readAt(ctx, location, offset.toLong(), len.toInt())

    override fun reserve(name: String, clip: Boolean): uniffi.threnody_ffi.ReservedFile? = Media.reserve(ctx, name)

    override fun freeSpace(): ULong? = try {
        android.os.StatFs(ctx.filesDir.path).availableBytes.toULong()
    } catch (_: Exception) {
        null
    }
}
