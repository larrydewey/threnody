package org.threnody.app

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import java.io.File

/**
 * Lends files the app keeps privately (received photos, and everything an
 * anonymous identity receives) to the app the user opens them with, one
 * read-only grant at a time. Not exported: other apps reach a file only
 * through a URI granted for it.
 *
 * Kept files are sealed: the other app reads a decrypted copy in the
 * cache, unlinked as soon as it is opened, so it is gone once that app
 * closes it.
 */
class FilesProvider : ContentProvider() {
    override fun onCreate() = true

    private fun file(uri: Uri): File? {
        val ctx = context ?: return null
        val f = File(ctx.filesDir, uri.path?.trimStart('/') ?: return null).canonicalFile
        val top = f.relativeTo(ctx.filesDir.canonicalFile).path.substringBefore('/')
        // Only the media folders, never identities or state.
        return f.takeIf { (top == "media" || top.startsWith("media-p-")) && it.isFile }
    }

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        if (mode != "r") throw SecurityException("read only")
        val f = file(uri) ?: throw java.io.FileNotFoundException(uri.toString())
        val ctx = context ?: throw java.io.FileNotFoundException(uri.toString())
        val copy: File? = try {
            Media.plainCopy(ctx, Uri.fromFile(f).toString())
        } catch (e: Exception) {
            throw java.io.FileNotFoundException("can't open ${f.name}: ${e.message}")
        }
        // Kept before sealing: as it is.
        val plain = copy ?: return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY)
        return try {
            ParcelFileDescriptor.open(plain, ParcelFileDescriptor.MODE_READ_ONLY)
        } finally {
            // Readable through the descriptor until it is closed, by no name.
            plain.delete()
        }
    }

    override fun getType(uri: Uri): String =
        MimeTypeMap.getSingleton().getMimeTypeFromExtension(uri.path?.substringAfterLast('.')?.lowercase())
            ?: "application/octet-stream"

    override fun query(uri: Uri, projection: Array<out String>?, s: String?, a: Array<out String>?, o: String?): Cursor? {
        val f = file(uri) ?: return null
        // Files are kept as "<time>-<random>-<name>": show the name.
        val name = f.name.split('-', limit = 3).getOrElse(2) { f.name }
        return MatrixCursor(arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)).apply {
            val size = try { uniffi.threnody_ffi.mediaSize(f.path).toLong() } catch (_: Exception) { f.length() }
            addRow(arrayOf<Any>(name, size))
        }
    }

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, s: String?, a: Array<out String>?) = 0
    override fun update(uri: Uri, v: ContentValues?, s: String?, a: Array<out String>?) = 0

    companion object {
        /** A shareable URI for a privately kept `file://` location; others pass through. */
        fun shareable(ctx: Context, location: Uri): Uri {
            if (location.scheme != "file") return location
            val rel = File(location.path ?: return location).relativeTo(ctx.filesDir).path
            return Uri.Builder().scheme("content").authority("${ctx.packageName}.files").path(rel).build()
        }
    }
}
