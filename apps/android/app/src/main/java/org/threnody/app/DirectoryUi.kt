package org.threnody.app

import android.app.Activity
import android.content.Context
import android.text.InputType
import android.text.format.DateUtils
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.Toast
import java.util.concurrent.Executor

/**
 * Relay directories (Appendix P): lists of volunteer relays and the
 * anonymous tokens that pay them. Subscribing shows a directory this
 * phone's address, never who you are. Every identity, anonymous ones
 * included, subscribes through links of its own.
 */
object DirectoryUi {
    const val SCHEME = "threnody-dir://"

    /** Subscribes the main identity and every anonymous one. */
    fun subscribe(a: Activity, worker: Executor, link: String, done: () -> Unit = {}) {
        Toast.makeText(a, "Subscribing…", Toast.LENGTH_SHORT).show()
        worker.execute {
            val msg = try {
                val d = Threnody.start(a).subscribeDirectory(link)
                for (id in Threnody.personaIds()) {
                    try { Threnody.personaNode(id)?.subscribeDirectory(link) } catch (_: Exception) {}
                }
                "Subscribed: ${d.relays} relays, ${d.tokens} tokens"
            } catch (e: Exception) {
                "Couldn't subscribe: ${e.message}"
            }
            a.runOnUiThread {
                Toast.makeText(a, msg, Toast.LENGTH_LONG).show()
                done()
            }
        }
    }

    /** Lists subscriptions; tap one to unsubscribe, or add one. */
    fun show(a: Activity, worker: Executor, prefill: String? = null) {
        worker.execute {
            val node = Threnody.start(a)
            val dirs = node.directories()
            val usable = node.volunteerRelayCount()
            a.runOnUiThread {
                val rows = dirs.map { d ->
                    val until = d.validUntilMs?.let {
                        "valid until " + DateUtils.formatDateTime(a, it.toLong(), DateUtils.FORMAT_SHOW_TIME)
                    } ?: "no relay list yet"
                    "Directory ${d.id}\n${d.relays} relays · ${d.tokens} tokens · $until"
                }
                val b = SecureBuilder(a)
                    .setTitle("Relay directories")
                    .setPositiveButton("Add") { _, _ -> add(a, worker, prefill) }
                    .setNegativeButton("Close", null)
                if (dirs.isEmpty()) {
                    b.setMessage(
                        "A directory lists volunteer relays and gives out anonymous tokens to pay them. " +
                            "When your contacts can't relay for you, circuits go through two volunteers. " +
                            "Subscribing shows it this phone's address, never who you are.",
                    )
                } else {
                    b.setTitle("Relay directories · $usable relays usable")
                    b.setItems(rows.toTypedArray()) { _, i ->
                        SecureBuilder(a)
                            .setTitle("Unsubscribe from directory ${dirs[i].id}?")
                            .setPositiveButton("Unsubscribe") { _, _ ->
                                worker.execute {
                                    node.unsubscribeDirectory(dirs[i].idHex)
                                    for (id in Threnody.personaIds()) {
                                        Threnody.personaNode(id)?.unsubscribeDirectory(dirs[i].idHex)
                                    }
                                }
                            }
                            .setNegativeButton("Cancel", null)
                            .show()
                    }
                }
                b.show()
                if (prefill != null && dirs.none { it.link == prefill }) add(a, worker, prefill)
            }
        }
    }

    fun add(a: Activity, worker: Executor, prefill: String?) {
        val field = EditText(a).apply {
            hint = "$SCHEME…"
            setText(prefill ?: "")
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            wrapping(newlines = false, max = 4)
        }
        SecureBuilder(a)
            .setTitle("Subscribe to a directory")
            .setMessage("Paste or scan its link. A relay is used only if enough of your directories list it.")
            .setView(LinearLayout(a).apply {
                setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
                addView(field, matchWrap)
            })
            .setPositiveButton("Subscribe") { _, _ ->
                val link = field.text.toString().trim()
                if (link.startsWith(SCHEME)) subscribe(a, worker, link)
                else Toast.makeText(a, "That isn't a $SCHEME link", Toast.LENGTH_LONG).show()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Subscribes a newly opened anonymous identity to the main identity's directories. */
    fun followMain(ctx: Context, persona: uniffi.threnody_ffi.ThrenodyNode) {
        val main = Threnody.start(ctx)
        val have = persona.directories().map { it.link }.toSet()
        for (d in main.directories()) {
            if (d.link !in have) try { persona.subscribeDirectory(d.link) } catch (_: Exception) {}
        }
    }
}
