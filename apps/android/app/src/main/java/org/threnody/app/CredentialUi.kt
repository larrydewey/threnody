package org.threnody.app

import android.app.Activity
import android.text.InputType
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.Toast
import uniffi.threnody_ffi.CredentialAskRecord
import uniffi.threnody_ffi.CredentialOfferRecord
import uniffi.threnody_ffi.ProfileAttr
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Zero-knowledge credentials (Appendix O): someone vouches for attributes
 * of a contact, who later proves just the ones they choose. Every step
 * that gives something away asks the user first, and nothing is shown
 * unless ticked.
 */
object CredentialUi {
    private fun attrs(list: List<ProfileAttr>) = list.joinToString("\n") { "${it.key}: ${it.value}" }

    private fun box(a: Activity) = LinearLayout(a).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
    }

    private fun field(a: Activity, hint: String, multiLine: Boolean = false) = EditText(a).apply {
        this.hint = hint
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        wrapping(newlines = multiLine, max = if (multiLine) 10 else 4)
        if (multiLine) minLines = 3
    }

    private fun toast(a: Activity, msg: String) = a.runOnUiThread { Toast.makeText(a, msg, Toast.LENGTH_LONG).show() }

    /** The credentials this identity holds; tap one to delete it. */
    fun list(a: Activity, node: ThrenodyNode) {
Threading.background {
            val held = node.credentials()
            val names = held.map { "${it.schema} · from ${Threnody.nameOf(node, it.issuer)}\n${attrs(it.attributes)}" }
            a.runOnUiThread {
                val b = SecureBuilder(a).setTitle("Credentials").setNegativeButton("Close", null)
                if (held.isEmpty()) {
                    b.setMessage("None yet. A contact can vouch for you from their chat's menu → Offer a credential.")
                } else {
                    b.setItems(names.toTypedArray()) { _, i ->
                        SecureBuilder(a)
                            .setTitle("Delete this credential?")
                            .setMessage(names[i])
                            .setPositiveButton("Delete") { _, _ -> Threading.background { node.deleteCredential(held[i].id) } }
                            .setNegativeButton("Cancel", null)
                            .show()
                    }
                }
                b.show()
            }
        }
    }

    /** Vouches for attributes of [peer] with a credential they keep. */
    fun offer(a: Activity, node: ThrenodyNode, peer: String, title: String) {
        val v = box(a)
        val schema = field(a, "Kind, e.g. hackspace/member")
        val body = field(a, "One per line: key=value", multiLine = true).apply { setText("name=\nmember=yes") }
        val days = field(a, "Valid for (days)").apply {
            inputType = InputType.TYPE_CLASS_NUMBER
            setText("365")
        }
        v.addView(a.label("You vouch for these. $title can later prove any of them to others, without showing the rest.", 14f, R.color.muted), matchWrap)
        v.addView(schema, matchWrap)
        v.addView(body, matchWrap)
        v.addView(days, matchWrap)
        SecureBuilder(a)
            .setTitle("Offer $title a credential")
            .setView(ScrollView(a).apply { addView(v) })
            .setPositiveButton("Offer") { _, _ ->
                val list = body.text.lines()
                    .mapNotNull { l -> l.split('=', limit = 2).takeIf { it.size == 2 } }
                    .map { (k, x) -> k.trim() to x.trim() }
                    .filter { (k, x) -> k.isNotEmpty() && x.isNotEmpty() }
                    .map { (k, x) -> ProfileAttr(k, x) }
                val kind = schema.text.toString().trim()
                if (kind.isEmpty() || list.isEmpty()) return@setPositiveButton toast(a, "A kind and at least one attribute are needed")
                val n = days.text.toString().toUIntOrNull() ?: 365u
                Threading.background {
                    try {
                        node.offerCredential(peer, kind, list, n)
                        toast(a, "Offered. $title decides whether to accept.")
                    } catch (e: Exception) {
                        toast(a, "Couldn't offer it: ${e.message}")
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Asks [peer] to prove attributes of a credential. */
    fun ask(a: Activity, node: ThrenodyNode, peer: String, title: String) {
        val v = box(a)
        val schema = field(a, "Kind, e.g. hackspace/member")
        val keys = field(a, "Attributes, comma-separated (optional)")
        v.addView(a.label("$title chooses whether to answer, and with what. You learn only what they show.", 14f, R.color.muted), matchWrap)
        v.addView(schema, matchWrap)
        v.addView(keys, matchWrap)
        SecureBuilder(a)
            .setTitle("Ask $title to prove something")
            .setView(v)
            .setPositiveButton("Ask") { _, _ ->
                val kind = schema.text.toString().trim()
                if (kind.isEmpty()) return@setPositiveButton
                val wanted = keys.text.split(',').map { it.trim() }.filter { it.isNotEmpty() }
                Threading.background {
                    try {
                        node.askCredential(peer, kind, wanted)
                        toast(a, "Asked")
                    } catch (e: Exception) {
                        toast(a, "Couldn't ask: ${e.message}")
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** An offer made to us: accept (keep the credential) or decline. */
    fun answerOffer(a: Activity, node: ThrenodyNode, offer: CredentialOfferRecord, done: () -> Unit) {
        val who = Threnody.nameOf(node, offer.peer)
        SecureBuilder(a)
            .setTitle("$who offers you a credential")
            .setMessage("${offer.schema}\n\n${attrs(offer.attributes)}\n\nKeep it to prove these later, choosing what to show each time.")
            .setPositiveButton("Accept") { _, _ ->
                Threading.background {
                    try { node.acceptCredentialOffer(offer.id) } catch (e: Exception) { toast(a, "Couldn't accept: ${e.message}") }
                    done()
                }
            }
            .setNegativeButton("Decline") { _, _ ->
                Threading.background {
                    try { node.declineCredential(offer.id) } catch (e: Exception) {
                        Threnody.say("! decline offer: ${e.message}")
                    }
                    done()
                }
            }
            .setNeutralButton("Later", null)
            .show()
    }

/** A request for a proof: pick a credential and what to show, or decline. */
    fun answerAsk(a: Activity, node: ThrenodyNode, ask: CredentialAskRecord, done: () -> Unit) {
        Threading.background {
            val who = Threnody.nameOf(node, ask.peer)
            val matching = node.credentials().filter { it.schema == ask.schema }
            a.runOnUiThread {
                if (matching.isEmpty()) {
                    SecureBuilder(a)
                        .setTitle("$who asks for a credential (${ask.schema})")
                        .setMessage("You don't hold one.")
                        .setPositiveButton("Decline") { _, _ ->
                            Threading.background {
                                try { node.declineCredential(ask.id) } catch (e: Exception) {
                                    Threnody.say("! decline ask: ${e.message}")
                                }
                                done()
                            }
                        }
                        .setNeutralButton("Later", null)
                        .show()
                    return@runOnUiThread
                }
                val v = box(a)
                v.addView(a.label("From your ${ask.schema} credential. Choose what to show; nothing else is revealed.", 14f, R.color.muted), matchWrap)
                val group = RadioGroup(a)
                matching.forEachIndexed { i, c ->
                    group.addView(RadioButton(a).apply {
                        id = i + 1
                        text = "From ${Threnody.nameOf(node, c.issuer)}"
                        isChecked = i == 0
                    })
                }
                v.addView(group, matchWrap)
                // Nothing is shown unless ticked.
                val checks = ask.keys.map { k -> CheckBox(a).apply { text = k }.also { v.addView(it, matchWrap) } }
                if (ask.keys.isEmpty()) v.addView(a.label("Only that you hold one.", 14f, R.color.muted), matchWrap)
                SecureBuilder(a)
                    .setTitle("$who asks you to prove something")
                    .setView(ScrollView(a).apply { addView(v) })
                    .setPositiveButton("Prove") { _, _ ->
                        val cred = matching[(group.checkedRadioButtonId - 1).coerceIn(0, matching.size - 1)]
                        val keys = ask.keys.filterIndexed { i, _ -> checks[i].isChecked }
                        Threading.background {
                            try {
                                node.presentCredential(ask.id, cred.id, keys)
                                toast(a, "Proof sent")
                            } catch (e: Exception) {
                                toast(a, "Couldn't prove it: ${e.message}")
                            }
                            done()
                        }
                    }
                    .setNegativeButton("Decline") { _, _ ->
                        Threading.background {
                            try { node.declineCredential(ask.id) } catch (e: Exception) {
                                Threnody.say("! decline ask (prove): ${e.message}")
                            }
                            done()
                        }
                    }
                    .setNeutralButton("Later", null)
                    .show()
            }
        }
    }

    /** What a contact proved to us. */
    fun showProof(a: Activity, node: ThrenodyNode, peer: String, issuer: String, schema: String, shown: List<ProfileAttr>, pseudonym: String) {
        val who = Threnody.nameOf(node, peer)
        val body = (if (shown.isEmpty()) "Nothing else was shown." else attrs(shown)) +
            "\n\nIssued by ${Threnody.nameOf(node, issuer)}.\nTheir pseudonym for you: ${pseudonym.take(16)}"
        SecureBuilder(a)
            .setTitle("$who proved a credential ($schema)")
            .setMessage(body)
            .setPositiveButton("OK", null)
            .show()
    }
}
