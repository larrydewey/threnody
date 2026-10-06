package org.threnody.app

import android.content.Context
import android.net.Uri
import android.os.Bundle
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import android.view.inputmethod.InputContentInfo
import android.widget.EditText

/**
 * The message field. It also takes GIFs and stickers the keyboard offers
 * (Gboard's GIF search, say): `content` gets each one's URI and type, and
 * read access to it lasts until `release` is called. The app itself
 * contacts no GIF service.
 */
class ComposeField(
    ctx: Context,
    private val content: (uri: Uri, mime: String, release: () -> Unit) -> Unit,
) : EditText(ctx) {
    override fun onCreateInputConnection(attrs: EditorInfo): InputConnection? {
        val ic = super.onCreateInputConnection(attrs) ?: return null
        attrs.contentMimeTypes = MIME
        return object : InputConnectionWrapper(ic, false) {
            override fun commitContent(info: InputContentInfo, flags: Int, opts: Bundle?): Boolean {
                val mime = MIME.firstOrNull { info.description.hasMimeType(it) } ?: return false
                if (flags and InputConnection.INPUT_CONTENT_GRANT_READ_URI_PERMISSION != 0) {
                    try { info.requestPermission() } catch (_: Exception) { return false }
                }
                content(info.contentUri, mime) { info.releasePermission() }
                return true
            }
        }
    }

    companion object {
        private val MIME = arrayOf("image/gif", "image/webp", "image/png", "image/jpeg")
    }
}
