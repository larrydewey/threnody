package org.threnody.app

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.text.InputType
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.Toast
import uniffi.threnody_ffi.ThrenodyNode

/**
 * A handle (Appendix S): `name-XXXX-XXXX`, which people can type into
 * "Add a contact" to find this phone without an invite. Off unless chosen:
 * anyone who has it can reach this phone and learn its IP address.
 */
object Handles {
    private const val PREFS = "handle"
    private const val NAME = "name"
    private const val VERSION = "version"

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private fun name(ctx: Context): String? = prefs(ctx).getString(NAME, null)
    private fun version(ctx: Context): Long = prefs(ctx).getLong(VERSION, 0)

    /** Publishes the chosen handle again after a restart (main identity only). */
    fun resume(ctx: Context, node: ThrenodyNode) {
        val n = name(ctx) ?: return
        Threading.background {
            try {
                node.setHandle(n, version(ctx).toULong())
            } catch (e: Exception) {
                Threnody.say("! handle: ${e.message}")
            }
        }
    }

    /** Shows our handle, or offers to make one. */
    fun show(a: Activity, node: ThrenodyNode) {
        val current = node.handle()
        if (current == null) choose(a, node, null) else showHandle(a, node, current)
    }

    private fun choose(a: Activity, node: ThrenodyNode, prefill: String?) {
        val field = EditText(a).apply {
            hint = "A name, e.g. larry"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            setText(prefill ?: "")
        }
        val box = LinearLayout(a).apply {
            setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
            addView(field)
        }
        SecureBuilder(a)
            .setTitle("Get a handle")
            .setMessage(
                "People who have your handle can find you by typing it, without an invite. " +
                    "A code made from your key is added to the name, so no one else can take it. " +
                    "Anyone with the handle can write to you (as a message request) and learn this " +
                    "phone's IP address. You can change or remove it at any time." +
                    if (!Privacy.reachInternet(a)) "\n\nNeeds “Reach contacts over the internet”, which is off." else "",
            )
            .setView(box)
            .setPositiveButton("Create") { _, _ -> publish(a, node, field.text.toString().trim(), version(a)) }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun publish(a: Activity, node: ThrenodyNode, name: String, version: Long) {
        if (name.isEmpty()) return
        Threading.background {
            try {
                val h = node.setHandle(name, version.toULong()) ?: return@background
                prefs(a).edit().putString(NAME, name).putLong(VERSION, version).apply()
                a.runOnUiThread { showHandle(a, node, h) }
            } catch (e: Exception) {
                a.runOnUiThread { Toast.makeText(a, "Couldn't make that handle: ${e.message}", Toast.LENGTH_LONG).show() }
            }
        }
    }

    private fun showHandle(a: Activity, node: ThrenodyNode, handle: String) {
        SecureBuilder(a)
            .setTitle(handle)
            .setMessage(
                "Give this to people you want to find you. They type it into “Add a contact”. " +
                    "A new code stops the old one from working within the hour.",
            )
            .setPositiveButton("Share") { _, _ ->
                a.startActivity(Intent.createChooser(
                    Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, handle), "Your handle"))
            }
            .setNeutralButton("Copy") { _, _ ->
                a.getSystemService(ClipboardManager::class.java)?.setPrimaryClip(ClipData.newPlainText("Handle", handle))
                Toast.makeText(a, "Copied", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("Change…") { _, _ -> change(a, node) }
            .show()
    }

    private fun change(a: Activity, node: ThrenodyNode) {
        val options = arrayOf("New code (same name)", "Different name", "Remove my handle")
        SecureBuilder(a)
            .setTitle("Change your handle")
            .setItems(options) { _, i ->
                when (i) {
                    0 -> publish(a, node, name(a) ?: return@setItems, version(a) + 1)
                    1 -> choose(a, node, name(a))
                    else -> Threading.background {
                        node.setHandle(null, 0u)
                        prefs(a).edit().remove(NAME).apply()
                        a.runOnUiThread { Toast.makeText(a, "Handle removed; it stops working within the hour", Toast.LENGTH_LONG).show() }
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }
}
