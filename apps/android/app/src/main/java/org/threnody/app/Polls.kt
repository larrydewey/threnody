package org.threnody.app

import android.app.Activity
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
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import uniffi.threnody_ffi.PollInfo

/** Polls in groups: asking one, and showing one in a bubble. */
object Polls {
    private const val MAX_OPTIONS = 12

    /** Asks for a question and its options; `send` gets them and whether several may be chosen. */
    fun compose(a: Activity, send: (question: String, options: List<String>, multi: Boolean) -> Unit) {
        val dp = a.resources.displayMetrics.density
        fun field(hint: String) = EditText(a).apply {
            this.hint = hint
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
        }
        val question = field("Question")
        val options = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }
        fun addOption() {
            if (options.childCount < MAX_OPTIONS) options.addView(field("Option ${options.childCount + 1}"))
        }
        addOption()
        addOption()
        val more = Button(a).apply {
            text = "Add an option"
            setOnClickListener { addOption(); if (options.childCount >= MAX_OPTIONS) visibility = View.GONE }
        }
        val multi = CheckBox(a).apply { text = "Allow several choices" }
        val form = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            val pad = (20 * dp).toInt()
            setPadding(pad, (8 * dp).toInt(), pad, 0)
            addView(question)
            addView(options)
            addView(more, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            addView(multi)
        }
        SecureBuilder(a)
            .setTitle("New poll")
            .setView(ScrollView(a).apply { addView(form) })
            .setPositiveButton("Send") { _, _ ->
                val opts = (0 until options.childCount)
                    .map { (options.getChildAt(it) as EditText).text.toString().trim() }
                    .filter { it.isNotEmpty() }
                val q = question.text.toString().trim()
                if (q.isEmpty() || opts.size < 2) {
                    android.widget.Toast.makeText(a, "A poll needs a question and at least two options.",
                        android.widget.Toast.LENGTH_LONG).show()
                } else {
                    send(q, opts, multi.isChecked)
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /**
     * A poll's options, each with its share of the votes; tapping one
     * calls `vote` with our new choices (unless it's closed).
     */
    fun view(a: Activity, p: PollInfo, fg: Int, accent: Int, vote: (List<UInt>) -> Unit): View {
        val dp = a.resources.displayMetrics.density
        val box = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }
        val status = listOfNotNull(
            if (p.multi) "Choose any" else "Choose one",
            "${p.voters} voted",
            if (p.closed) "closed" else null,
        ).joinToString(" · ")
        box.addView(TextView(a).apply {
            text = status
            textSize = 12f
            setTextColor(fg)
            alpha = 0.7f
            setPadding(0, 0, 0, (4 * dp).toInt())
        })
        val mine = p.options.indices.filter { p.options[it].mine }.map { it.toUInt() }
        for ((i, o) in p.options.withIndex()) {
            val row = LinearLayout(a).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(0, (6 * dp).toInt(), 0, (6 * dp).toInt())
                minimumWidth = (220 * dp).toInt()
            }
            row.addView(LinearLayout(a).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                addView(TextView(a).apply {
                    text = (if (o.mine) "✓ " else "") + o.text
                    textSize = 15f
                    setTextColor(fg)
                    if (o.mine) setTypeface(typeface, Typeface.BOLD)
                }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                addView(TextView(a).apply {
                    text = o.votes.toString()
                    textSize = 13f
                    setTextColor(fg)
                    alpha = 0.7f
                })
            }, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            row.addView(ProgressBar(a, null, android.R.attr.progressBarStyleHorizontal).apply {
                max = maxOf(1, p.voters.toInt())
                progress = o.votes.toInt()
                progressTintList = android.content.res.ColorStateList.valueOf(accent)
            }, LinearLayout.LayoutParams(MATCH_PARENT, (6 * dp).toInt()).apply { topMargin = (4 * dp).toInt() })
            row.contentDescription = "${o.text}: ${o.votes} vote${if (o.votes == 1u) "" else "s"}" +
                (if (o.mine) ", your choice" else "")
            if (!p.closed) {
                val choice = i.toUInt()
                val tap = View.OnClickListener {
                    vote(
                        when {
                            p.multi && choice in mine -> mine - choice
                            p.multi -> mine + choice
                            choice in mine -> emptyList()
                            else -> listOf(choice)
                        },
                    )
                }
                // The bubble's long press makes its text take touches, so the
                // option's text and bar vote too, not only the row's edges.
                fun all(v: View) {
                    v.setOnClickListener(tap)
                    if (v is android.view.ViewGroup) for (c in 0 until v.childCount) all(v.getChildAt(c))
                }
                all(row)
            }
            box.addView(row, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        return box
    }
}
