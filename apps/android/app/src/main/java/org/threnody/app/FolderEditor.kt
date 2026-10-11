package org.threnody.app

import android.app.Activity
import android.app.TimePickerDialog
import android.graphics.Typeface
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.ArrayAdapter
import android.widget.TextView
import android.widget.ToggleButton
import uniffi.threnody_ffi.FolderInfo
import uniffi.threnody_ffi.NotifyLevel
import uniffi.threnody_ffi.QuietSlotInfo

/**
 * Makes or changes a chat folder: its name and emoji, the chats in it,
 * its rules, when it notifies, its quiet hours, and who may break
 * through them.
 */
object FolderEditor {
    const val RULE_CONTACTS = 1u
    const val RULE_GROUPS = 2u
    const val RULE_VERIFIED = 4u
    const val RULE_UNREAD = 8u
    const val RULE_MENTIONS = 16u
    private val DAYS = listOf("M", "T", "W", "T", "F", "S", "S")
    private val DAY_NAMES = listOf("Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday")

    /**
     * `chats` is every chat that can go in (key to title); `save` gets the
     * folder (id 0 if new).
     */
    fun show(a: Activity, folder: FolderInfo?, chats: List<Pair<String, String>>, save: (FolderInfo) -> Unit) {
        val dp = a.resources.displayMetrics.density
        fun px(v: Int) = (v * dp).toInt()
        fun heading(t: String) = TextView(a).apply {
            text = t
            setTypeface(typeface, Typeface.BOLD)
            setPadding(0, px(16), 0, px(4))
        }
        val form = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(px(20), px(8), px(20), 0)
        }
        val emoji = EditText(a).apply {
            hint = "🙂"
            setText(folder?.emoji ?: "")
            gravity = Gravity.CENTER
        }
        val name = EditText(a).apply {
            hint = "Name (Family, Work…)"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
            setText(folder?.name ?: "")
        }
        form.addView(LinearLayout(a).apply {
            addView(emoji, LinearLayout.LayoutParams(px(64), WRAP_CONTENT))
            addView(name, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        })

        // Chats picked by hand.
        form.addView(heading("Chats"))
        val picked = (folder?.chats ?: emptyList()).toMutableSet()
        val pickedLabel = TextView(a)
        fun showPicked() {
            pickedLabel.text = if (picked.isEmpty()) "None picked" else
                chats.filter { it.first in picked }.joinToString(", ") { it.second }
        }
        showPicked()
        form.addView(pickedLabel)
        form.addView(Button(a).apply {
            text = "Choose chats"
            setOnClickListener {
                val keys = chats.map { it.first }
                val on = keys.map { it in picked }.toBooleanArray()
                SecureBuilder(a)
                    .setTitle("Chats in this folder")
                    .setMultiChoiceItems(chats.map { it.second }.toTypedArray(), on) { _, i, checked ->
                        if (checked) picked += keys[i] else picked -= keys[i]
                    }
                    .setPositiveButton("Done") { _, _ -> showPicked() }
                    .show()
            }
        }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))

        // Rules.
        form.addView(heading("Also include"))
        val rules = folder?.rules ?: 0u
        fun rule(label: String, bit: UInt) = CheckBox(a).apply {
            text = label
            isChecked = rules and bit != 0u
            tag = bit
        }
        val ruleBoxes = listOf(
            rule("All contacts", RULE_CONTACTS),
            rule("All groups", RULE_GROUPS),
            rule("Verified contacts", RULE_VERIFIED),
        )
        ruleBoxes.forEach { form.addView(it) }
        form.addView(heading("Only show"))
        val keepBoxes = listOf(
            rule("Chats with unread messages", RULE_UNREAD),
            rule("Chats that mention me", RULE_MENTIONS),
        )
        keepBoxes.forEach { form.addView(it) }

        // Notifications.
        form.addView(heading("Notifications"))
        val levels = listOf<Pair<String, NotifyLevel?>>(
            "Each chat's own setting" to null,
            "All messages" to NotifyLevel.ALL,
            "Only mentions and replies to me" to NotifyLevel.MENTIONS,
            "Off" to NotifyLevel.OFF,
        )
        val notify = Spinner(a).apply {
            adapter = ArrayAdapter(a, android.R.layout.simple_spinner_dropdown_item, levels.map { it.first })
            setSelection(levels.indexOfFirst { it.second == folder?.notify }.coerceAtLeast(0))
        }
        form.addView(notify)

        // Quiet hours.
        form.addView(heading("Quiet hours"))
        form.addView(TextView(a).apply {
            text = "Messages still arrive; they just don't notify."
            alpha = 0.7f
        })
        val slots = (folder?.quiet ?: emptyList()).toMutableList()
        val slotList = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }
        fun time(m: Int) = "%d:%02d".format(m / 60 % 24, m % 60)
        lateinit var drawSlots: () -> Unit
        drawSlots = {
            slotList.removeAllViews()
            for ((i, s) in slots.withIndex()) {
                val row = LinearLayout(a).apply {
                    orientation = LinearLayout.VERTICAL
                    setPadding(0, px(6), 0, px(6))
                }
                row.addView(LinearLayout(a).apply {
                    for (d in 0 until 7) {
                        addView(ToggleButton(a).apply {
                            textOn = DAYS[d]
                            textOff = DAYS[d]
                            isChecked = s.days.toInt() and (1 shl d) != 0
                            contentDescription = DAY_NAMES[d]
                            setOnCheckedChangeListener { _, on ->
                                val days = if (on) s.days.toInt() or (1 shl d) else s.days.toInt() and (1 shl d).inv()
                                slots[i] = slots[i].copy(days = days.toUByte())
                            }
                        }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                    }
                })
                fun pick(start: Boolean) {
                    val cur = (if (start) slots[i].start else slots[i].end).toInt()
                    TimePickerDialog(a, { _, h, m ->
                        val v = (h * 60 + m).toUShort()
                        slots[i] = if (start) slots[i].copy(start = v) else slots[i].copy(end = v)
                        drawSlots()
                    }, cur / 60 % 24, cur % 60, android.text.format.DateFormat.is24HourFormat(a)).show()
                }
                row.addView(LinearLayout(a).apply {
                    gravity = Gravity.CENTER_VERTICAL
                    addView(Button(a).apply { text = "From ${time(s.start.toInt())}"; setOnClickListener { pick(true) } })
                    addView(Button(a).apply { text = "to ${time(s.end.toInt())}"; setOnClickListener { pick(false) } })
                    addView(Button(a).apply {
                        text = "Remove"
                        setOnClickListener { slots.removeAt(i); drawSlots() }
                    })
                })
                slotList.addView(row)
            }
        }
        drawSlots()
        form.addView(slotList)
        form.addView(Button(a).apply {
            text = "Add quiet hours"
            setOnClickListener {
                // Weeknights, 22:00 to 7:00, to start from.
                slots += QuietSlotInfo(days = 0x7Fu, start = (22 * 60).toUShort(), end = (7 * 60).toUShort())
                drawSlots()
            }
        }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))

        // Letting messages through.
        form.addView(heading("Let messages through"))
        val through = CheckBox(a).apply {
            text = "When someone writes to me directly again and again"
            isChecked = (folder?.breakthroughCount ?: 0u) > 0u
        }
        form.addView(through)
        fun number(v: Int, hint: String) = EditText(a).apply {
            inputType = InputType.TYPE_CLASS_NUMBER
            setText(v.toString())
            this.hint = hint
            gravity = Gravity.CENTER
        }
        val count = number((folder?.breakthroughCount ?: 0u).toInt().takeIf { it > 0 } ?: 2, "times")
        val window = number((folder?.breakthroughWindowMin ?: 0u).toInt().takeIf { it > 0 } ?: 10, "minutes")
        form.addView(LinearLayout(a).apply {
            gravity = Gravity.CENTER_VERTICAL
            addView(count, LinearLayout.LayoutParams(px(64), WRAP_CONTENT))
            addView(TextView(a).apply { text = " messages within " })
            addView(window, LinearLayout.LayoutParams(px(64), WRAP_CONTENT))
            addView(TextView(a).apply { text = " minutes" })
        })
        form.addView(TextView(a).apply {
            text = "Directly: in a chat with you, or mentioning or replying to you in a group. A chat you turned off stays off."
            alpha = 0.7f
            setPadding(0, 0, 0, px(8))
        })

        SecureBuilder(a)
            .setTitle(if (folder == null) "New folder" else "Edit folder")
            .setView(ScrollView(a).apply { addView(form, MATCH_PARENT, WRAP_CONTENT) })
            .setPositiveButton("Save") { _, _ ->
                val n = name.text.toString().trim()
                if (n.isEmpty()) {
                    android.widget.Toast.makeText(a, "A folder needs a name.", android.widget.Toast.LENGTH_SHORT).show()
                    return@setPositiveButton
                }
                val r = (ruleBoxes + keepBoxes).filter { it.isChecked }.fold(0u) { acc, b -> acc or (b.tag as UInt) }
                save(
                    FolderInfo(
                        id = folder?.id ?: 0uL,
                        name = n,
                        emoji = emoji.text.toString().trim(),
                        chats = picked.toList(),
                        rules = r,
                        notify = levels[notify.selectedItemPosition].second,
                        quiet = slots.filter { it.days.toInt() != 0 },
                        breakthroughCount = if (through.isChecked)
                            (count.text.toString().toIntOrNull() ?: 2).coerceIn(1, 20).toUByte() else 0u,
                        breakthroughWindowMin = (window.text.toString().toIntOrNull() ?: 10).coerceIn(1, 1440).toUShort(),
                    ),
                )
            }
            .setNegativeButton("Cancel", null)
            .show()
    }
}
