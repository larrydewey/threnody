package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.text.format.DateFormat
import android.text.format.Formatter
import android.util.Patterns
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.GridLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Toast
import java.util.Date
import java.util.concurrent.Executors
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.ThrenodyNode

/**
 * A conversation's photos, files and links at a glance, newest first, so
 * finding one doesn't mean scrolling the chat. It reads the stored
 * history, so it holds whatever hasn't disappeared.
 */
class MediaActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var node: ThrenodyNode
    private lateinit var content: LinearLayout
    private lateinit var tabs: LinearLayout
    private var entries: List<HistoryEntry> = emptyList()
    private var tab = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        val persona = intent.getStringExtra(ChatActivity.PERSONA)
        val group = intent.getStringExtra(ChatActivity.GROUP)
        val device = intent.getStringExtra(ChatActivity.DEVICE)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val bar = TopBar(this) { finish() }.apply {
            title.text = "Media, files and links"
            subtitle.text = intent.getStringExtra(TITLE) ?: ""
            subtitle.visibility = View.VISIBLE
        }
        tabs = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(12), dp(4), dp(12), dp(8))
        }
        content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(12), dp(4), dp(12), dp(12))
        }
        val scroll = ScrollView(this).apply { addView(content, MATCH_PARENT, WRAP_CONTENT) }
        root.addView(bar, matchWrap)
        root.addView(tabs, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
        fitSystemBars(root, bar, scroll)
        tab = savedInstanceState?.getInt("tab") ?: 0

        worker.execute {
            node = try { Threnody.node(this, persona) } catch (_: Exception) { return@execute runOnUiThread { finish() } }
            entries = try {
                when {
                    group != null -> node.groupHistory(group, 10_000u)
                    device != null -> node.history(device, 10_000u)
                    else -> emptyList()
                }
            } catch (_: Exception) {
                emptyList()
            }.asReversed()
            runOnUiThread { show() }
        }
    }

    override fun onSaveInstanceState(out: Bundle) {
        super.onSaveInstanceState(out)
        out.putInt("tab", tab)
    }

    private val photos get() = entries.filter { e -> e.file?.let { Media.isImage(it.name) } == true }
    private val files get() = entries.filter { e -> e.file?.let { !Media.isImage(it.name) } == true }

    /** Every link in the messages' text, with the message it came from. */
    private val links: List<Pair<String, HistoryEntry>>
        get() = entries.flatMap { e ->
            val m = Patterns.WEB_URL.matcher(e.text)
            buildList { while (m.find()) add(m.group() to e) }
        }

    private fun show() {
        val counts = listOf(photos.size, files.size, links.size)
        tabs.removeAllViews()
        listOf("Photos", "Files", "Links").forEachIndexed { i, name ->
            tabs.addView(label("$name · ${counts[i]}", 14f, if (i == tab) R.color.on_accent else R.color.text).apply {
                gravity = Gravity.CENTER
                setPadding(dp(12), dp(8), dp(12), dp(8))
                background = rounded(color(if (i == tab) R.color.accent else R.color.surface), dp(18).toFloat())
                setOnClickListener { tab = i; show() }
            }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginEnd = if (i < 2) dp(6) else 0 })
        }
        content.removeAllViews()
        when (tab) {
            0 -> photoGrid()
            1 -> fileRows()
            else -> linkList()
        }
    }

    private fun empty(what: String) {
        content.addView(label("No $what in this conversation" +
            " (disappearing messages take theirs with them).", 14f, R.color.muted).apply {
            gravity = Gravity.CENTER
            setPadding(dp(16), dp(48), dp(16), 0)
        }, matchWrap)
    }

    private fun photoGrid() {
        val list = photos
        if (list.isEmpty()) return empty("photos")
        val columns = 3
        val side = (resources.displayMetrics.widthPixels - dp(24) - dp(4) * (columns - 1)) / columns
        val grid = GridLayout(this).apply { columnCount = columns }
        for ((i, e) in list.withIndex()) {
            val f = e.file ?: continue
            val location = f.location
            val cell: View = if (f.sensitive || location == null) {
                // Covered here too, and never decoded until opened.
                label(if (location == null) "📷" else "🔒", 22f, R.color.on_cover).apply {
                    gravity = Gravity.CENTER
                    background = rounded(color(R.color.cover), dp(6).toFloat()).apply { setStroke(dp(1), color(R.color.cover_edge)) }
                    contentDescription = if (location == null) "Photo not available" else "Sensitive photo"
                }
            } else {
                ImageView(this).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = rounded(color(R.color.surface), dp(6).toFloat())
                    clipToOutline = true
                    contentDescription = e.text.ifBlank { f.name }
                    Media.thumbnail(this@MediaActivity, location, side) { b -> runOnUiThread { setImageBitmap(b) } }
                }
            }
            if (location != null) {
                cell.setOnClickListener {
                    startActivity(Intent(this, ImageActivity::class.java)
                        .putExtra(ImageActivity.LOCATION, location)
                        .putExtra(ImageActivity.NAME, f.name)
                        .putExtra(ImageActivity.CAPTION, e.text))
                }
            }
            grid.addView(cell, GridLayout.LayoutParams().apply {
                width = side
                height = side
                setMargins(0, 0, if (i % columns < columns - 1) dp(4) else 0, dp(4))
            })
        }
        content.addView(grid, matchWrap)
    }

    private fun fileRows() {
        val list = files
        if (list.isEmpty()) return empty("files")
        for (e in list) {
            val f = e.file ?: continue
            row(
                (if (f.sensitive) "📎 Sensitive file: " else "📎 ") + f.name,
                Formatter.formatShortFileSize(this, f.size.toLong()) + " · " + date(e) +
                    (if (e.text.isNotBlank()) " · ${e.text}" else ""),
            ) {
                val uri = f.location?.let(Uri::parse) ?: return@row toast("This file isn't on this device")
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, FilesProvider.shareable(this, uri))
                        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
                } catch (_: Exception) {
                    toast("No app can open this file")
                }
            }
        }
    }

    private fun linkList() {
        val list = links
        if (list.isEmpty()) return empty("links")
        for ((url, e) in list) {
            row(url, date(e) + if (e.outgoing) " · you" else "") { openLink(url) }
        }
    }

    /** Links leave the app (and show your address to that site), so ask first. */
    private fun openLink(url: String) {
        val uri = Uri.parse(if (url.contains("://")) url else "https://$url")
        AlertDialog.Builder(this)
            .setTitle("Open this link?")
            .setMessage("${uri}\n\nIt opens in your browser, outside Threnody. The site sees your network address.")
            .setPositiveButton("Open") { _, _ ->
                try { startActivity(Intent(Intent.ACTION_VIEW, uri)) } catch (_: Exception) { toast("No app can open this link") }
            }
            .setNeutralButton("Copy") { _, _ ->
                getSystemService(android.content.ClipboardManager::class.java)
                    ?.setPrimaryClip(android.content.ClipData.newPlainText("link", url))
                toast("Copied")
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun row(title: String, detail: String, onClick: () -> Unit) {
        content.addView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(10), dp(8), dp(10))
            background = getDrawable(android.R.drawable.list_selector_background)
            addView(label(title, 15f).apply { maxLines = 2 })
            addView(label(detail, 12f, R.color.muted).apply { maxLines = 2 })
            setOnClickListener { onClick() }
        }, matchWrap)
    }

    private fun date(e: HistoryEntry): String {
        val d = Date(e.atMs.toLong())
        return DateFormat.getMediumDateFormat(this).format(d) + " " + DateFormat.getTimeFormat(this).format(d)
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    override fun onDestroy() {
        worker.shutdownNow()
        super.onDestroy()
    }

    companion object {
        const val TITLE = "title"
    }
}

