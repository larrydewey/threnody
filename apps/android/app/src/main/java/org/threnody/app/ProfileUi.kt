package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.text.InputType
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.Toast
import java.util.concurrent.Executor
import uniffi.threnody_ffi.ProfileAttr
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Profiles (spec §4.3): what an identity says about itself, and which of
 * it each contact sees. Nothing is shared until chosen per contact.
 */
object ProfileUi {
    /** Suggested details; any other key works too. */
    private val SUGGESTED = listOf("name", "email", "phone", "about")

    /** Lists the profile's details; tap one to change or remove it, or add one. */
    fun edit(a: Activity, node: ThrenodyNode, title: String, worker: Executor) {
        worker.execute {
            val attrs = node.profile()
            a.runOnUiThread {
                // A message and a list can't share an AlertDialog: build both.
                val box = LinearLayout(a).apply {
                    orientation = LinearLayout.VERTICAL
                    setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
                }
                box.addView(a.label(
                    if (attrs.isEmpty()) "Nothing yet. Contacts see only the details you share with each of them."
                    else "Contacts see only the details you share with each of them (chat ⋮ → Share your profile).",
                    14f, R.color.muted,
                ), matchWrap)
                lateinit var dialog: AlertDialog
                fun row(text: String, color: Int, onClick: () -> Unit) = box.addView(a.label(text, 16f, color).apply {
                    minHeight = a.dp(48)
                    gravity = android.view.Gravity.CENTER_VERTICAL
                    background = a.ripple()
                    setOnClickListener { dialog.dismiss(); onClick() }
                }, matchWrap)
                for (attr in attrs) row("${attr.key}: ${attr.value}", R.color.text) { detail(a, node, title, worker, attr) }
                row("+ Add a detail", R.color.accent) { detail(a, node, title, worker, null) }
                dialog = SecureBuilder(a)
                    .setTitle(title)
                    .setView(android.widget.ScrollView(a).apply { addView(box) })
                    .setNegativeButton("Done", null)
                    .show()
            }
        }
    }

    /** Adds a detail ([current] null) or changes or removes one. */
    private fun detail(a: Activity, node: ThrenodyNode, title: String, worker: Executor, current: ProfileAttr?) {
        val key = field(a, "What (name, email, …)", current?.key).apply { isEnabled = current == null }
        val value = field(a, "Value", current?.value)
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
            if (current == null) {
                addView(a.label("Suggestions: ${SUGGESTED.joinToString(", ")}", 12f, R.color.muted), matchWrap)
            }
            addView(key, matchWrap)
            addView(value, matchWrap)
        }
        fun save(remove: Boolean) = worker.execute {
            val k = key.text.toString().trim().lowercase()
            val v = value.text.toString().trim()
            val attrs = node.profile().toMutableList()
            attrs.removeAll { it.key == k }
            if (!remove && k.isNotEmpty() && v.isNotEmpty()) {
                val at = node.profile().indexOfFirst { it.key == k }
                attrs.add(if (at < 0) attrs.size else minOf(at, attrs.size), ProfileAttr(k, v))
            }
            val error = try { node.setProfile(attrs); null } catch (e: Exception) { e.message }
            a.runOnUiThread {
                if (error != null) Toast.makeText(a, "Couldn't save: $error", Toast.LENGTH_LONG).show()
                edit(a, node, title, worker)
            }
        }
        SecureBuilder(a)
            .setTitle(if (current == null) "Add a detail" else current.key)
            .setView(box)
            .setPositiveButton("Save") { _, _ -> save(false) }
            .apply { if (current != null) setNeutralButton("Remove") { _, _ -> save(true) } }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Chooses which profile details [peer] sees. */
    fun share(a: Activity, node: ThrenodyNode, peer: String, name: String, worker: Executor, editTitle: String) {
        worker.execute {
            val attrs = node.profile()
            val shared = try { node.sharedWith(peer).toSet() } catch (_: Exception) { emptySet() }
            a.runOnUiThread {
                if (attrs.isEmpty()) {
                    SecureBuilder(a)
                        .setTitle("Your profile is empty")
                        .setMessage("Add details such as your name first; then choose which ones $name sees.")
                        .setPositiveButton("Add details") { _, _ -> edit(a, node, editTitle, worker) }
                        .setNegativeButton("Cancel", null)
                        .show()
                    return@runOnUiThread
                }
                val checked = BooleanArray(attrs.size) { attrs[it].key in shared }
                SecureBuilder(a)
                    .setTitle("What $name sees")
                    .setMultiChoiceItems(attrs.map { "${it.key}: ${it.value}" }.toTypedArray(), checked) { _, i, on ->
                        checked[i] = on
                    }
                    .setPositiveButton("Save") { _, _ ->
                        val keys = attrs.filterIndexed { i, _ -> checked[i] }.map { it.key }
                        worker.execute {
                            val error = try { node.setSharedWith(peer, keys); null } catch (e: Exception) { e.message }
                            a.runOnUiThread {
                                Toast.makeText(
                                    a,
                                    error?.let { "Couldn't share: $it" } ?: if (keys.isEmpty()) "$name sees none of your profile"
                                    else "$name sees your ${keys.joinToString(", ")}. They may keep what they've seen.",
                                    Toast.LENGTH_LONG,
                                ).show()
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun field(a: Activity, hint: String, value: String?) = EditText(a).apply {
        this.hint = hint
        setText(value ?: "")
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        wrapping(newlines = false, max = 4)
    }
}
