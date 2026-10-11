package org.threnody.app

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.ColorStateList
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.widget.FrameLayout
import android.widget.ImageView
import kotlin.math.abs
import kotlin.math.min

/**
 * Holds a message row; dragging it to the right (as in Signal) shows a
 * reply arrow, and letting go past the threshold calls [onReply]. Vertical
 * drags still scroll the chat, and taps and long presses reach the row.
 */
@SuppressLint("ViewConstructor")
class SwipeToReply(context: Context, private val row: View, tint: Int, private val onReply: () -> Unit) :
    FrameLayout(context) {
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private val threshold = 64 * resources.displayMetrics.density
    private val most = 96 * resources.displayMetrics.density
    private val arrow = ImageView(context).apply {
        setImageResource(R.drawable.ic_reply)
        imageTintList = ColorStateList.valueOf(tint)
        alpha = 0f
    }
    private var downX = 0f
    private var downY = 0f
    private var dragging = false
    private var armed = false

    init {
        val side = (28 * resources.displayMetrics.density).toInt()
        addView(arrow, LayoutParams(side, side, Gravity.START or Gravity.CENTER_VERTICAL).apply {
            marginStart = (8 * resources.displayMetrics.density).toInt()
        })
        addView(row, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
    }

    override fun onInterceptTouchEvent(ev: MotionEvent): Boolean {
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = ev.x
                downY = ev.y
                dragging = false
            }
            MotionEvent.ACTION_MOVE -> {
                val dx = ev.x - downX
                val dy = ev.y - downY
                if (!dragging && dx > slop && dx > 2 * abs(dy)) {
                    dragging = true
                    parent?.requestDisallowInterceptTouchEvent(true)
                    // The row's long press mustn't fire mid-swipe.
                    row.cancelLongPress()
                }
            }
        }
        return dragging
    }

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(ev: MotionEvent): Boolean {
        when (ev.actionMasked) {
            MotionEvent.ACTION_MOVE -> {
                val dx = min(maxOf(0f, ev.x - downX), most)
                row.translationX = dx
                arrow.alpha = min(1f, dx / threshold)
                val now = dx >= threshold
                if (now && !armed) Design.mediumHaptic(this)
                armed = now
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (armed && ev.actionMasked == MotionEvent.ACTION_UP) onReply()
                armed = false
                dragging = false
                row.animate().translationX(0f).setDuration(Design.durationFast.toLong()).start()
                arrow.animate().alpha(0f).setDuration(Design.durationFast.toLong()).start()
            }
        }
        return true
    }
}
