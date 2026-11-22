package org.threnody.app

import android.app.Activity
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.ColorFilter
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.graphics.drawable.BitmapDrawable
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.LayerDrawable
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import uniffi.threnody_ffi.BackgroundInfo
import uniffi.threnody_ffi.PatternInfo
import uniffi.threnody_ffi.PictureInfo
import uniffi.threnody_ffi.ShapeInfo
import uniffi.threnody_ffi.ThemeInfo

/**
 * Chat themes (Appendix W): how a chat's shared look is drawn here, and
 * the sheet that chooses one. The same shapes and colours as on every
 * other device; in the dark theme, light backgrounds are deepened.
 */
object Themes {
    /** Accent colours on offer (the same on every app). */
    val ACCENTS = listOf(
        0xFF4F46E5, 0xFF1E6FB8, 0xFF00897B, 0xFF2E7D32, 0xFFE65100, 0xFFC62828, 0xFFD81B60, 0xFF6A1B9A,
    ).map { it.toInt() }

    private fun Int.u() = toUInt()
    private fun UInt.i() = toInt()

    /** `c` for this theme (dark: light colours deepened, as the core does). */
    private fun shade(ctx: Context, c: UInt): Int =
        if (ctx.isNight) uniffi.threnody_ffi.nightColor(c).i() else c.i()

    /** A pattern's ink at night: lighter, so it shows on the deepened paper. */
    private fun inkFor(ctx: Context, ink: UInt): Int {
        val c = ink.i()
        if (!ctx.isNight) return c
        val mix = { v: Int -> v + (255 - v) / 2 }
        return Color.argb(Color.alpha(c), mix(Color.red(c)), mix(Color.green(c)), mix(Color.blue(c)))
    }

    /** The chat's background, or null to keep the app's own. */
    fun background(ctx: Context, theme: ThemeInfo?, own: Boolean, tileDp: Int = 132): Drawable? {
        if (theme == null || own) return null
        return when (val b = theme.background) {
            is BackgroundInfo.Plain -> null
            is BackgroundInfo.Color -> ColorDrawable(shade(ctx, b.color))
            is BackgroundInfo.Gradient -> GradientDrawable(
                GradientDrawable.Orientation.TOP_BOTTOM, intArrayOf(shade(ctx, b.top), shade(ctx, b.bottom)),
            )
            is BackgroundInfo.Pattern -> PatternDrawable(ctx, b.pattern, inkFor(ctx, b.ink), shade(ctx, b.paper), tileDp)
            is BackgroundInfo.Photo -> photo(ctx, b.data)
        }
    }

    /** A photo behind the messages: cropped to fill, and dimmed so text reads. */
    private fun photo(ctx: Context, data: ByteArray): Drawable? {
        val bmp = BitmapFactory.decodeByteArray(data, 0, data.size) ?: return null
        val pic = object : BitmapDrawable(ctx.resources, bmp) {
            override fun draw(canvas: Canvas) {
                val b = bounds
                val scale = maxOf(b.width().toFloat() / bmp.width, b.height().toFloat() / bmp.height)
                val w = bmp.width * scale
                val h = bmp.height * scale
                canvas.save()
                canvas.clipRect(b)
                canvas.translate(b.left + (b.width() - w) / 2, b.top + (b.height() - h) / 2)
                canvas.scale(scale, scale)
                canvas.drawBitmap(bmp, 0f, 0f, paint)
                canvas.restore()
            }
        }
        val scrim = ColorDrawable(if (ctx.isNight) 0x99000000.toInt() else 0x66FFFFFF)
        return LayerDrawable(arrayOf(pic, scrim))
    }

    /** The chat's accent, or null for the app's own. */
    fun accent(theme: ThemeInfo?, own: Boolean): Int? = if (own) null else theme?.accent?.i()

    /** Text on `c`: white on a dark colour, near black on a light one. */
    fun onColor(c: Int): Int {
        val light = 0.299 * Color.red(c) + 0.587 * Color.green(c) + 0.114 * Color.blue(c)
        return if (light > 160) 0xFF1B1B1F.toInt() else Color.WHITE
    }

    /** Shows a group's picture on `a` (emoji on its colour, or the photo); false if it has none. */
    fun showPicture(a: Avatar, picture: PictureInfo?): Boolean {
        when (picture) {
            is PictureInfo.Emoji -> {
                a.text = picture.emoji
                a.background = GradientDrawable().apply {
                    shape = GradientDrawable.OVAL
                    setColor(picture.color.i())
                }
            }
            is PictureInfo.Photo -> {
                val bmp = BitmapFactory.decodeByteArray(picture.data, 0, picture.data.size) ?: return false
                a.text = ""
                a.background = androidx.core.graphics.drawable.RoundedBitmapDrawableFactory
                    .create(a.resources, bmp).apply { isCircular = true }
            }
            null -> return false
        }
        return true
    }

    /**
     * A pattern drawn from its shapes: the tile (`tileDp` across) filled
     * in `ink`, repeated over `paper`.
     */
    class PatternDrawable(ctx: Context, pattern: PatternInfo, ink: Int, private val paper: Int, tileDp: Int = 132) : Drawable() {
        private val tile = ctx.dp(tileDp).toFloat()
        private val shapes = uniffi.threnody_ffi.patternShapes(pattern)
        private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ink; style = Paint.Style.FILL }
        private val line = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = ink
            style = Paint.Style.STROKE
            strokeCap = Paint.Cap.ROUND
        }
        private val bitmap: Bitmap by lazy {
            val n = tile.toInt().coerceAtLeast(1)
            Bitmap.createBitmap(n, n, Bitmap.Config.ARGB_8888).also { b ->
                val c = Canvas(b)
                c.drawColor(paper)
                // Shapes that cross an edge come in again at the other.
                for (dx in -1..1) for (dy in -1..1) {
                    c.save()
                    c.translate(dx * tile, dy * tile)
                    draw(c, tile)
                    c.restore()
                }
            }
        }
        private val tiles = Paint().apply {
            shader = android.graphics.BitmapShader(bitmap, android.graphics.Shader.TileMode.REPEAT, android.graphics.Shader.TileMode.REPEAT)
        }

        private fun draw(c: Canvas, s: Float) {
            for (sh in shapes) when (sh) {
                is ShapeInfo.Circle -> c.drawCircle(sh.x * s, sh.y * s, sh.r * s, fill)
                is ShapeInfo.Line -> {
                    line.strokeWidth = sh.width * s
                    c.drawLine(sh.x1 * s, sh.y1 * s, sh.x2 * s, sh.y2 * s, line)
                }
                is ShapeInfo.Polygon -> {
                    val p = Path()
                    sh.xs.indices.forEach { i ->
                        if (i == 0) p.moveTo(sh.xs[i] * s, sh.ys[i] * s) else p.lineTo(sh.xs[i] * s, sh.ys[i] * s)
                    }
                    p.close()
                    c.drawPath(p, fill)
                }
            }
        }

        override fun draw(canvas: Canvas) {
            canvas.drawRect(bounds, tiles)
        }
        override fun setAlpha(alpha: Int) { tiles.alpha = alpha }
        override fun setColorFilter(colorFilter: ColorFilter?) { tiles.colorFilter = colorFilter }
        @Deprecated("Deprecated in Java")
        override fun getOpacity() = PixelFormat.OPAQUE
    }

    /**
     * The sheet that chooses a chat's look, with a preview of it on top.
     * `canSet` false (a group member who isn't an owner or admin): only
     * keeping one's own look. `set` gets the chosen theme (null: plain).
     */
    fun pick(
        a: Activity,
        current: ThemeInfo?,
        group: Boolean,
        canSet: Boolean,
        own: Boolean,
        title: String,
        onPhoto: (picture: Boolean, done: (ByteArray) -> Unit) -> Unit,
        setOwn: (Boolean) -> Unit,
        set: (ThemeInfo?) -> Unit,
    ) {
        var chosen = current ?: ThemeInfo(BackgroundInfo.Plain, null, null)
        lateinit var sheet: android.app.Dialog
        val body = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }

        // The preview: the background, a bubble each way, and the picture.
        val preview = FrameLayout(a).apply {
            clipToOutline = true
            outlineProvider = android.view.ViewOutlineProvider.BACKGROUND
        }
        val face = Avatar(a, 40)
        val theirs = a.label("Merry Christmas! 🎄", 14f).apply { setPadding(a.dp(12), a.dp(8), a.dp(12), a.dp(8)) }
        val mine = a.label("Same to you!", 14f).apply { setPadding(a.dp(12), a.dp(8), a.dp(12), a.dp(8)) }
        val bubbles = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(14), a.dp(14), a.dp(14), a.dp(14))
            if (group) addView(face, LinearLayout.LayoutParams(a.dp(40), a.dp(40)).apply { bottomMargin = a.dp(8) })
            addView(theirs, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            addView(mine, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { gravity = Gravity.END; topMargin = a.dp(6) })
        }
        preview.addView(bubbles, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        fun redraw() {
            val bg = background(a, chosen, false, tileDp = 84) ?: ColorDrawable(a.color(R.color.background))
            preview.background = bg
            preview.outlineProvider = object : android.view.ViewOutlineProvider() {
                override fun getOutline(v: View, o: android.graphics.Outline) = o.setRoundRect(0, 0, v.width, v.height, a.dp(20).toFloat())
            }
            theirs.background = rounded(a.color(R.color.bubble_in), a.dp(16).toFloat())
            theirs.setTextColor(a.color(R.color.text))
            val acc = chosen.accent?.i() ?: a.color(R.color.bubble_out)
            mine.background = rounded(acc, a.dp(16).toFloat())
            mine.setTextColor(if (chosen.accent != null) onColor(acc) else a.color(R.color.on_bubble_out))
            if (!showPicture(face, chosen.picture)) face.show(title, title)
        }
        body.addView(a.label(if (group) "Group theme" else "Chat theme", 20f, weight = Design.weightBold).apply {
            setPadding(a.dp(4), 0, 0, a.dp(Design.md))
        })
        body.addView(preview, LinearLayout.LayoutParams(MATCH_PARENT, a.dp(if (group) 170 else 120)))

        fun heading(t: String) = body.addView(a.label(t, 14f, R.color.accent, Design.weightBold).apply {
            setPadding(a.dp(4), a.dp(Design.lg), 0, a.dp(Design.sm))
        })
        fun row(vararg views: View): HorizontalScrollView {
            val r = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL }
            views.forEach { v ->
                val size = v.layoutParams
                r.addView(v, LinearLayout.LayoutParams(size?.width ?: WRAP_CONTENT, size?.height ?: WRAP_CONTENT).apply { marginEnd = a.dp(10) })
            }
            return HorizontalScrollView(a).apply { isHorizontalScrollBarEnabled = false; addView(r) }
        }
        fun swatch(d: Drawable, label: String, onClick: () -> Unit) = View(a).apply {
            background = d
            clipToOutline = true
            outlineProvider = object : android.view.ViewOutlineProvider() {
                override fun getOutline(v: View, o: android.graphics.Outline) = o.setOval(0, 0, v.width, v.height)
            }
            foreground = a.ripple(borderless = true)
            contentDescription = label
            layoutParams = LinearLayout.LayoutParams(a.dp(48), a.dp(48))
            setOnClickListener { v -> Design.lightHaptic(v); onClick(); redraw() }
        }

        if (canSet) {
            heading("Theme packs")
            val packs = uniffi.threnody_ffi.themePacks().map { p ->
                LinearLayout(a).apply {
                    orientation = LinearLayout.VERTICAL
                    gravity = Gravity.CENTER_HORIZONTAL
                    contentDescription = p.name
                    background = a.ripple()
                    addView(FrameLayout(a).apply {
                        background = background(a, p.theme, false, tileDp = 44)
                        clipToOutline = true
                        outlineProvider = object : android.view.ViewOutlineProvider() {
                            override fun getOutline(v: View, o: android.graphics.Outline) = o.setRoundRect(0, 0, v.width, v.height, a.dp(16).toFloat())
                        }
                        addView(a.label(p.emoji, 26f).apply { gravity = Gravity.CENTER }, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
                    }, LinearLayout.LayoutParams(a.dp(72), a.dp(72)))
                    addView(a.label(p.name, 12f).apply { gravity = Gravity.CENTER; setPadding(0, a.dp(4), 0, 0) })
                    setOnClickListener { v ->
                        Design.lightHaptic(v)
                        chosen = if (group) p.theme else p.theme.copy(picture = null)
                        redraw()
                    }
                }
            }
            body.addView(row(*packs.toTypedArray()))

            heading("Patterns")
            val patterns = uniffi.threnody_ffi.themePatterns().map { pat ->
                val ink = (chosen.accent?.i() ?: a.color(R.color.accent)).let { (it and 0x00FFFFFF) or 0x40000000 }
                val paper = 0xFFF3F4F8.toInt()
                swatch(PatternDrawable(a, pat, inkFor(a, ink.u()), shade(a, paper.u()), tileDp = 48), uniffi.threnody_ffi.patternName(pat)) {
                    val acc = chosen.accent?.i() ?: Themes.ACCENTS[0]
                    chosen = chosen.copy(background = BackgroundInfo.Pattern(pat, ((acc and 0x00FFFFFF) or 0x40000000).u(), paper.u()))
                }
            }
            body.addView(row(*patterns.toTypedArray()))

            heading("Colours")
            val colors = uniffi.threnody_ffi.themeColors().map { c ->
                swatch(ColorDrawable(shade(a, c)), "Colour") { chosen = chosen.copy(background = BackgroundInfo.Color(c)) }
            }
            val g = uniffi.threnody_ffi.themeGradients()
            val gradients = (g.indices step 2).map { i ->
                swatch(GradientDrawable(GradientDrawable.Orientation.TOP_BOTTOM, intArrayOf(shade(a, g[i]), shade(a, g[i + 1]))), "Gradient") {
                    chosen = chosen.copy(background = BackgroundInfo.Gradient(g[i], g[i + 1]))
                }
            }
            val photo = swatch(ColorDrawable(a.color(R.color.field)), "Photo") {
                onPhoto(false) { bytes -> chosen = chosen.copy(background = BackgroundInfo.Photo(bytes)); redraw() }
            }.also { v -> (v as View).foreground = a.getDrawable(R.drawable.ic_image)?.mutate()?.apply { setTint(a.color(R.color.muted)) } }
            body.addView(row(*(colors + gradients + photo).toTypedArray()))

            heading("Accent")
            val none = swatch(GradientDrawable().apply { setColor(a.color(R.color.field)); setStroke(a.dp(2), a.color(R.color.divider)) }, "App's own") {
                chosen = chosen.copy(accent = null)
            }
            val accents = ACCENTS.map { c -> swatch(ColorDrawable(c), "Accent") { chosen = chosen.copy(accent = c.u()) } }
            body.addView(row(none, *accents.toTypedArray()))

            if (group) {
                heading("Picture")
                body.addView(LinearLayout(a).apply {
                    orientation = LinearLayout.HORIZONTAL
                    addView(a.secondaryButton("Emoji") {
                        EmojiPicker(a, "Group picture") { e ->
                            val field = (chosen.accent?.i() ?: a.color(R.color.accent)).let { (it and 0x00FFFFFF) or 0x33000000 }
                            chosen = chosen.copy(picture = PictureInfo.Emoji(e, androidx.core.graphics.ColorUtils.compositeColors(field, Color.WHITE).u()))
                            redraw()
                        }.show()
                    })
                    addView(a.secondaryButton("Photo") {
                        onPhoto(true) { bytes -> chosen = chosen.copy(picture = PictureInfo.Photo(bytes)); redraw() }
                    })
                    addView(a.secondaryButton("None") { chosen = chosen.copy(picture = null); redraw() })
                })
            }
        } else {
            body.addView(a.label("Only the group's owners and admins can change its theme.", 14f, R.color.muted, maxLines = 0).apply {
                setPadding(a.dp(4), a.dp(Design.lg), 0, 0)
            })
        }

        // One's own look, here only.
        body.addView(LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(a.dp(4), a.dp(Design.lg), 0, 0)
            addView(LinearLayout(a).apply {
                orientation = LinearLayout.VERTICAL
                addView(a.label("Keep my own look here", 16f))
                addView(a.label("The plain background on your devices, whatever the theme. No one else is told.", 13f, R.color.muted, maxLines = 0))
            }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            val sw = com.google.android.material.materialswitch.MaterialSwitch(
                android.view.ContextThemeWrapper(a, com.google.android.material.R.style.Theme_Material3_DayNight_NoActionBar),
            ).apply {
                isChecked = own
                setOnCheckedChangeListener { _, on -> setOwn(on) }
            }
            addView(sw)
        })

        if (canSet) body.addView(LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.END
            setPadding(0, a.dp(Design.lg), 0, 0)
            addView(a.tertiaryButton("Plain") { sheet.dismiss(); set(null) })
            addView(a.primaryButton("Set theme") { sheet.dismiss(); set(chosen) }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply {
                marginStart = a.dp(Design.sm)
            })
        })

        sheet = a.bottomSheet(android.widget.ScrollView(a).apply { addView(body) })
        sheet.show()
        preview.post { redraw() }
    }

    /**
     * Shrinks a picked photo to fit a theme: at most `side` pixels across,
     * as a JPEG under `max` bytes (stepping its quality down).
     */
    fun shrink(ctx: Context, uri: android.net.Uri, side: Int, max: Int): ByteArray? = try {
        val src = android.graphics.ImageDecoder.createSource(ctx.contentResolver, uri)
        val bmp = android.graphics.ImageDecoder.decodeBitmap(src) { d, info, _ ->
            val s = info.size
            val scale = minOf(1f, side.toFloat() / maxOf(s.width, s.height))
            d.setTargetSize((s.width * scale).toInt().coerceAtLeast(1), (s.height * scale).toInt().coerceAtLeast(1))
            d.allocator = android.graphics.ImageDecoder.ALLOCATOR_SOFTWARE
        }
        var q = 85
        var out: ByteArray
        do {
            val buf = java.io.ByteArrayOutputStream()
            bmp.compress(Bitmap.CompressFormat.JPEG, q, buf)
            out = buf.toByteArray()
            q -= 10
        } while (out.size > max && q > 20)
        out.takeIf { it.size <= max }
    } catch (_: Exception) {
        null
    }
}
