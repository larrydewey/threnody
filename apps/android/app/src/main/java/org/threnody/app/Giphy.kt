package org.threnody.app

import android.app.Activity
import android.content.Context
import android.text.Editable
import android.text.TextWatcher
import android.util.LruCache
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.BaseAdapter
import android.widget.EditText
import android.widget.GridView
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.util.concurrent.Executors

/**
 * GIF search through GIPHY's API. GIPHY sees what is searched for and this
 * device's IP address, so nothing is asked of it until the user agrees
 * ([Privacy.giphy]). The GIF picked is downloaded here and sent like a
 * photo: the contact's device never contacts GIPHY.
 *
 * The API key comes from the build (GIPHY_API_KEY in the environment or a
 * Gradle property), or from one the user enters, which wins.
 */
object Giphy {
    class Gif(val id: String, val title: String, val preview: String, val full: String)

    private const val PREFS = "giphy"
    private const val KEY = "api_key"
    private const val API = "https://api.giphy.com/v1/gifs"
    private const val LIMIT = 30
    private const val TIMEOUT_MS = 15_000
    /** Previews are small; anything bigger than this isn't one. */
    private const val MAX_PREVIEW = 2L * 1024 * 1024

    val net = Executors.newFixedThreadPool(4)
    private val previews = object : LruCache<String, ByteArray>(16 * 1024 * 1024) {
        override fun sizeOf(key: String, value: ByteArray) = value.size
    }

    fun key(ctx: Context): String =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(KEY, null)?.takeIf { it.isNotBlank() }
            ?: BuildConfig.GIPHY_API_KEY

    fun setKey(ctx: Context, key: String) =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putString(KEY, key.trim()).apply()

    /** Trending GIFs for an empty `query`, else matches. Blocks; call off the UI thread. */
    fun search(ctx: Context, query: String): List<Gif> {
        val key = URLEncoder.encode(key(ctx), "UTF-8")
        val url = if (query.isBlank()) "$API/trending?api_key=$key&limit=$LIMIT&rating=pg-13"
        else "$API/search?api_key=$key&limit=$LIMIT&rating=pg-13&q=" + URLEncoder.encode(query.trim(), "UTF-8")
        val body = JSONObject(String(get(url, 1024 * 1024)))
        val data = body.getJSONArray("data")
        return (0 until data.length()).mapNotNull { i ->
            val o = data.getJSONObject(i)
            val images = o.optJSONObject("images") ?: return@mapNotNull null
            fun url(vararg names: String) = names.firstNotNullOfOrNull { n ->
                images.optJSONObject(n)?.optString("url")?.takeIf { it.startsWith("https://") }
            }
            val preview = url("fixed_width_small", "fixed_width_downsampled", "fixed_width") ?: return@mapNotNull null
            val full = url("downsized", "fixed_width", "original") ?: return@mapNotNull null
            Gif(o.optString("id"), o.optString("title").ifBlank { "GIF" }, preview, full)
        }
    }

    /** A preview's bytes, cached. Blocks. */
    fun preview(url: String): ByteArray = previews.get(url) ?: get(url, MAX_PREVIEW).also { previews.put(url, it) }

    /** At most `max` bytes from `url` over HTTPS. Blocks. */
    fun get(url: String, max: Long): ByteArray {
        require(url.startsWith("https://")) { "not HTTPS" }
        val c = URL(url).openConnection() as HttpURLConnection
        try {
            c.connectTimeout = TIMEOUT_MS
            c.readTimeout = TIMEOUT_MS
            c.instanceFollowRedirects = true
            val code = c.responseCode
            if (code == 401 || code == 403) throw IllegalStateException("GIPHY refused the API key")
            if (code != 200) throw IllegalStateException("GIPHY answered $code")
            if (c.contentLengthLong > max) throw IllegalArgumentException("too large")
            return c.inputStream.use { input ->
                val out = java.io.ByteArrayOutputStream()
                val buf = ByteArray(16 * 1024)
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    out.write(buf, 0, n)
                    if (out.size() > max) throw IllegalArgumentException("too large")
                }
                out.toByteArray()
            }
        } finally {
            c.disconnect()
        }
    }
}

/**
 * GIFs from GIPHY in a tray from the bottom of the screen: trending ones,
 * or a search. `pick` gets the one tapped; the tray then closes. `phone`
 * picks from the GIFs on this phone instead.
 */
class GifPicker(
    private val a: Activity,
    private val phone: () -> Unit,
    private val pick: (Giphy.Gif) -> Unit,
) {
    private lateinit var dialog: android.app.Dialog
    private var gifs: List<Giphy.Gif> = emptyList()
    private val status = a.label("", 14f, R.color.muted).apply { gravity = Gravity.CENTER }
    private val grid = GridView(a)
    /** Bumped by each search, so a slow, older answer is dropped. */
    private var generation = 0
    private val handler = android.os.Handler(android.os.Looper.getMainLooper())
    private var pending: Runnable? = null

    private val adapter = object : BaseAdapter() {
        override fun getCount() = gifs.size
        override fun getItem(i: Int) = gifs[i]
        override fun getItemId(i: Int) = i.toLong()
        override fun getView(i: Int, convert: View?, parent: ViewGroup?): View {
            val g = gifs[i]
            val v = (convert as? ImageView) ?: ImageView(a).apply {
                scaleType = ImageView.ScaleType.CENTER_CROP
                background = rounded(a.color(R.color.surface), a.dp(8).toFloat())
                clipToOutline = true
                layoutParams = android.widget.AbsListView.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, a.dp(110))
            }
            if (v.tag == g.preview) return v
            v.tag = g.preview
            v.setImageDrawable(null)
            v.contentDescription = g.title
            Giphy.net.execute {
                val bytes = try { Giphy.preview(g.preview) } catch (_: Exception) { return@execute }
                Media.animated({ android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(bytes)) }, a.dp(160)) { d ->
                    a.runOnUiThread {
                        if (v.tag != g.preview || d == null) return@runOnUiThread
                        v.setImageDrawable(d)
                        (d as? android.graphics.drawable.AnimatedImageDrawable)?.start()
                    }
                }
            }
            return v
        }
    }

    fun show() {
        val search = EditText(a).apply {
            hint = "Search GIPHY"
            textSize = 16f
            wrapping(newlines = false, max = 1)
            addTextChangedListener(object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                override fun afterTextChanged(s: Editable) {
                    // Wait for a pause in typing: each search goes to GIPHY.
                    pending?.let(handler::removeCallbacks)
                    val q = s.toString()
                    pending = Runnable { load(q) }.also { handler.postDelayed(it, 400) }
                }
            })
        }
        grid.apply {
            numColumns = 3
            horizontalSpacing = a.dp(6)
            verticalSpacing = a.dp(6)
            stretchMode = GridView.STRETCH_COLUMN_WIDTH
            selector = android.graphics.drawable.ColorDrawable(android.graphics.Color.TRANSPARENT)
            this.adapter = this@GifPicker.adapter
            setOnItemClickListener { _, _, i, _ ->
                dialog.dismiss()
                pick(gifs[i])
            }
        }
        fun link(text: String, onClick: () -> Unit) = TextView(a).apply {
            this.text = text
            textSize = 15f
            setTypeface(typeface, android.graphics.Typeface.BOLD)
            setTextColor(a.color(R.color.accent))
            setPadding(a.dp(12), a.dp(8), a.dp(4), a.dp(8))
            setOnClickListener { onClick() }
        }
        val top = LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(a.label("GIFs", 18f).apply { setTypeface(typeface, android.graphics.Typeface.BOLD) },
                LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(link("On this phone") { dialog.dismiss(); phone() })
            addView(link("Close") { dialog.dismiss() })
        }
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(12), 0, a.dp(12), a.dp(4))
            addView(top, matchWrap)
            addView(search, matchWrap)
            addView(status, matchWrap.apply { topMargin = a.dp(4) })
            addView(grid, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, (a.resources.displayMetrics.heightPixels * 0.5).toInt()))
            // GIPHY's terms ask for this wherever its results are shown.
            addView(a.label("Powered by GIPHY", 12f, R.color.muted).apply {
                gravity = Gravity.END
                setPadding(0, a.dp(4), 0, 0)
            }, matchWrap)
        }
        dialog = a.bottomSheet(box)
        dialog.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_HIDDEN or
            WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
        dialog.setOnDismissListener { pending?.let(handler::removeCallbacks) }
        dialog.show()
        load("")
    }

    private fun load(query: String) {
        val mine = ++generation
        status.text = "Loading…"
        status.visibility = View.VISIBLE
        Giphy.net.execute {
            val result = try { Result.success(Giphy.search(a, query)) } catch (e: Exception) { Result.failure(e) }
            a.runOnUiThread {
                if (mine != generation) return@runOnUiThread
                gifs = result.getOrDefault(emptyList())
                adapter.notifyDataSetChanged()
                grid.setSelection(0)
                val error = result.exceptionOrNull()
                status.text = when {
                    error != null -> "Couldn't reach GIPHY: ${error.message ?: error.javaClass.simpleName}"
                    gifs.isEmpty() -> "No GIFs for “${query.trim()}”"
                    else -> ""
                }
                status.visibility = if (status.text.isEmpty()) View.GONE else View.VISIBLE
            }
        }
    }
}
