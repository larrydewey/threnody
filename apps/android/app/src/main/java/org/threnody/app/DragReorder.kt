package org.threnody.app

import android.annotation.SuppressLint
import android.view.MotionEvent
import android.view.View
import android.widget.LinearLayout

/**
 * Long-press-and-drag reordering for rows of a vertical [LinearLayout].
 * The pressed row lifts and follows the finger while the others make
 * room; letting go reports where it went. A long press that doesn't move
 * calls `still` instead (the row's menu).
 */
object DragReorder {
    /**
     * Makes `row` draggable among `rows()` (the reorderable rows of
     * `list`, in order). `drop(from, to)` gets indexes into `rows()`.
     */
    @SuppressLint("ClickableViewAccessibility")
    fun attach(
        list: LinearLayout,
        row: View,
        rows: () -> List<View>,
        drop: (from: Int, to: Int) -> Unit,
        still: () -> Unit,
    ) {
        var dragging = false
        var startY = 0f
        var lastY = 0f
        var moved = false
        var to = -1
        val slop = 12 * row.resources.displayMetrics.density
        row.setOnTouchListener { v, ev ->
            when (ev.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    startY = ev.rawY
                    lastY = ev.rawY
                    false
                }
                MotionEvent.ACTION_MOVE -> {
                    lastY = ev.rawY
                    if (!dragging) return@setOnTouchListener false
                    val all = rows()
                    val from = all.indexOf(v)
                    if (from < 0) return@setOnTouchListener true
                    val dy = ev.rawY - startY
                    if (kotlin.math.abs(dy) > slop) moved = true
                    v.translationY = dy
                    // Its new place: after every other row whose middle it passed.
                    val mid = v.top + v.height / 2f + dy
                    to = all.withIndex().count { (i, o) -> i != from && o.top + o.height / 2f < mid }
                    for ((i, other) in all.withIndex()) {
                        if (other === v) continue
                        val shift = when {
                            from < to && i in (from + 1)..to -> -v.height.toFloat()
                            from > to && i in to until from -> v.height.toFloat()
                            else -> 0f
                        }
                        if (other.translationY != shift) other.animate().translationY(shift).setDuration(120).start()
                    }
                    true
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    if (!dragging) return@setOnTouchListener false
                    dragging = false
                    list.parent?.requestDisallowInterceptTouchEvent(false)
                    val all = rows()
                    val from = all.indexOf(v)
                    for (other in all) {
                        other.animate().cancel()
                        other.translationY = 0f
                    }
                    v.translationZ = 0f
                    v.alpha = 1f
                    if (ev.actionMasked == MotionEvent.ACTION_UP) {
                        if (moved && from >= 0 && to >= 0 && to != from) drop(from, to) else if (!moved) still()
                    }
                    true
                }
                else -> false
            }
        }
        row.setOnLongClickListener { v ->
            // Lift it; the touch listener moves it from here.
            dragging = true
            moved = false
            to = -1
            startY = lastY
            v.translationZ = 8 * v.resources.displayMetrics.density
            v.alpha = 0.92f
            list.parent?.requestDisallowInterceptTouchEvent(true)
            Design.mediumHaptic(v)
            true
        }
    }
}
