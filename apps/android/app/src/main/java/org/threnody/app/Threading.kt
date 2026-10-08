package org.threnody.app

import android.content.Context
import java.util.concurrent.Executors
import java.util.concurrent.ThreadFactory
import java.util.concurrent.atomic.AtomicInteger

/**
 * Centralized threading for the app. One executor per purpose, shared
 * across the process. Activities submit work here instead of creating
 * their own executors. The process-wide singletons shut down when the
 * process dies (no explicit shutdown needed; the OS reclaims threads).
 */
object Threading {
    /** Background work: DB, crypto, node calls. Serial by default. */
    val background: java.util.concurrent.ExecutorService = Executors.newSingleThreadExecutor(named("threnody-bg"))

    /** I/O-bound work: file reads, image decoding, network fetches. Parallel. */
    val io: java.util.concurrent.ExecutorService = Executors.newFixedThreadPool(4, named("threnody-io"))

    /** Periodic maintenance: media sweep, redial, standby. */
    val scheduled: java.util.concurrent.ScheduledExecutorService = Executors.newSingleThreadScheduledExecutor(named("threnody-sched"))

    /** Runs a block on the background executor. */
    fun background(block: () -> Unit) = background.execute(block)

    /** Runs a block on the I/O executor. */
    fun io(block: () -> Unit) = io.execute(block)

    /** Schedules a periodic task on the scheduled executor. */
    fun schedulePeriodic(initialDelay: Long, period: Long, unit: java.util.concurrent.TimeUnit, block: () -> Unit) =
        scheduled.scheduleWithFixedDelay(block, initialDelay, period, unit)

    /** Schedules a one-shot task on the scheduled executor. */
    fun scheduleOnce(delay: Long, unit: java.util.concurrent.TimeUnit, block: () -> Unit) =
        scheduled.schedule(block, delay, unit)

    private fun named(prefix: String): ThreadFactory = ThreadFactory { r ->
        Thread(r, "$prefix-${ThreadId.next()}").apply { isDaemon = true }
    }

    private object ThreadId {
        private val counter = AtomicInteger(0)
        fun next() = counter.incrementAndGet()
    }

    /** Call from Application.onCreate if you want eager startup (optional). */
    fun warmUp(context: Context) {
        // Touch executors so threads are created early.
        background.execute { }
        io.execute { }
        scheduled.execute { }
    }
}