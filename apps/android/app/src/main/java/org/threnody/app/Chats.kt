package org.threnody.app

import android.app.Activity
import android.widget.Toast
import java.util.concurrent.Executor
import uniffi.threnody_ffi.ThrenodyNode

/** Clearing and deleting conversations, from a chat or the list. */
object Chats {
    /**
     * Deletes the messages but keeps the contact (or group): on all our
     * devices for a contact, on this one for a group.
     */
    fun clear(a: Activity, node: ThrenodyNode, worker: Executor, title: String, device: String?, group: String?, done: () -> Unit) {
        SecureBuilder(a)
            .setTitle("Clear chat with $title?")
            .setMessage(
                if (group != null) "Its messages are deleted from this device. You stay in the group, and members keep their copies."
                else "Your messages with $title are deleted from all your devices. They stay a contact, and you can keep talking. They keep their copy.",
            )
            .setPositiveButton("Clear") { _, _ ->
                worker.execute {
                    val error = try {
                        if (group != null) node.clearGroupConversation(group) else node.clearConversation(device!!)
                        null
                    } catch (e: Exception) { e.message }
                    a.runOnUiThread {
                        if (error != null) Toast.makeText(a, "Couldn't clear: $error", Toast.LENGTH_LONG).show()
                        done()
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Deletes the contact (every device of theirs) and the conversation, on all our devices. */
    fun delete(a: Activity, node: ThrenodyNode, worker: Executor, title: String, device: String, done: () -> Unit) {
        SecureBuilder(a)
            .setTitle("Delete $title?")
            .setMessage(
                "They leave your contacts and your messages with them are deleted, on all your devices. " +
                    "They keep their copy. If they write again, it arrives as a new message request.",
            )
            .setPositiveButton("Delete") { _, _ ->
                worker.execute {
                    val error = try { node.deleteConversation(device); null } catch (e: Exception) { e.message }
                    a.runOnUiThread {
                        if (error != null) Toast.makeText(a, "Couldn't delete: $error", Toast.LENGTH_LONG).show()
                        done()
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }
}
