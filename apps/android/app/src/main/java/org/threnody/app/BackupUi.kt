package org.threnody.app

import android.app.Activity
import android.content.Intent
import android.text.InputType
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.Toast
import uniffi.threnody_ffi.BackupInfo
import uniffi.threnody_ffi.ThrenodyNode
import uniffi.threnody_ffi.backupPassphraseMinChars
import uniffi.threnody_ffi.inspectBackup
import uniffi.threnody_ffi.stageRestore
import java.text.DateFormat
import java.util.Date

/**
 * Backups (Appendix C, *Backups*): the identity, contacts and history,
 * sealed under a passphrase the user chooses, saved wherever they pick.
 * A restore is staged beside the node's files and finished when the app
 * restarts, before the node opens (`Threnody.open`).
 */
object BackupUi {
    private fun box(a: Activity) = LinearLayout(a).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
    }

    private fun secret(a: Activity, hint: String) = EditText(a).apply {
        this.hint = hint
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
    }

    private fun toast(a: Activity, msg: String) = a.runOnUiThread { Toast.makeText(a, msg, Toast.LENGTH_LONG).show() }

    /** Picks where to save, then asks for a passphrase and writes the backup there. */
    fun backUp(a: Activity, node: ThrenodyNode) {
        val day = java.text.SimpleDateFormat("yyyy-MM-dd", java.util.Locale.ROOT).format(Date())
        val pick = Intent(Intent.ACTION_CREATE_DOCUMENT)
            .addCategory(Intent.CATEGORY_OPENABLE)
            .setType("application/octet-stream")
            .putExtra(Intent.EXTRA_TITLE, "threnody-$day.backup")
        ActivityResultRegistry.get(a).launch(pick) { code, data ->
            val uri = data?.data
            if (code == Activity.RESULT_OK && uri != null) askNewPassphrase(a, node, uri)
        }
    }

    private fun askNewPassphrase(a: Activity, node: ThrenodyNode, uri: android.net.Uri) {
        val min = backupPassphraseMinChars().toInt()
        val first = secret(a, "Passphrase")
        val second = secret(a, "Again")
        SecureBuilder(a)
            .setTitle("Back up")
            .setMessage("Choose a passphrase of at least $min characters. Anyone with the backup " +
                "and its passphrase can become you, and nobody can open it without the passphrase.")
            .setView(box(a).apply { addView(first); addView(second) })
            .setPositiveButton("Save") { _, _ ->
                val pw = first.text.toString()
                when {
                    pw.length < min -> toast(a, "The passphrase needs at least $min characters")
                    pw != second.text.toString() -> toast(a, "The passphrases differ")
                    else -> Threading.background {
                        try {
                            val data = node.exportBackup(pw)
                            a.contentResolver.openOutputStream(uri, "wt")?.use { it.write(data) }
                                ?: throw IllegalStateException("can't write there")
                            toast(a, "Backup saved. Keep it and its passphrase safe.")
                        } catch (e: Exception) {
                            toast(a, "Couldn't back up: ${e.message}")
                        }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Picks a backup, opens it with its passphrase, and asks before replacing anything. */
    fun restore(a: Activity, node: ThrenodyNode) {
        val pick = Intent(Intent.ACTION_OPEN_DOCUMENT)
            .addCategory(Intent.CATEGORY_OPENABLE)
            .setType("*/*")
        ActivityResultRegistry.get(a).launch(pick) { code, data ->
            val uri = data?.data
            if (code != Activity.RESULT_OK || uri == null) return@launch
            Threading.background {
                val bytes = try {
                    a.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                } catch (e: Exception) {
                    null
                }
                a.runOnUiThread {
                    if (bytes == null) toast(a, "Couldn't read that file") else askPassphrase(a, node, bytes)
                }
            }
        }
    }

    private fun askPassphrase(a: Activity, node: ThrenodyNode, bytes: ByteArray) {
        val field = secret(a, "Passphrase")
        SecureBuilder(a)
            .setTitle("Restore from a backup")
            .setMessage("Type the backup's passphrase.")
            .setView(box(a).apply { addView(field) })
            .setPositiveButton("Open") { _, _ ->
                val pw = field.text.toString()
                Threading.background {
                    try {
                        val info = inspectBackup(bytes, pw)
                        a.runOnUiThread { confirm(a, node, bytes, pw, info) }
                    } catch (e: Exception) {
                        toast(a, "Couldn't open the backup: ${e.message}")
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun confirm(a: Activity, node: ThrenodyNode, bytes: ByteArray, pw: String, info: BackupInfo) {
        val made = DateFormat.getDateTimeInstance(DateFormat.LONG, DateFormat.SHORT)
            .format(Date(info.createdMs.toLong()))
        val same = info.fingerprint == node.deviceFingerprint()
        SecureBuilder(a)
            .setTitle("Replace this phone's identity?")
            .setMessage("This backup of ${Threnody.short(info.fingerprint)} was made $made.\n\n" +
                (if (same) "It is this phone's own identity: anything newer than the backup is lost. " else "") +
                "Restoring removes this phone's identity, contacts, history and anonymous identities, " +
                "and puts the backup's in their place. Restore only when the device the backup came " +
                "from is gone: to use two devices as one, link them instead.\n\n" +
                "Threnody restarts to finish.")
            .setPositiveButton("Restore") { _, _ ->
                Threading.background {
                    try {
                        stageRestore(Threnody.home(a), bytes, pw, KeyVault.secret(a))
                        a.runOnUiThread { Threnody.restartApp(a) }
                    } catch (e: Exception) {
                        toast(a, "Couldn't restore: ${e.message}")
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }
}
