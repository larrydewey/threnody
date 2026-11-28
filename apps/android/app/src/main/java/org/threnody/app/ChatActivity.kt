package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Typeface
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.text.format.DateFormat
import android.text.format.DateUtils
import android.text.format.Formatter
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.EditText
import android.widget.ImageButton
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.SeekBar
import android.widget.TextView
import android.widget.Toast
import java.util.Date
import uniffi.threnody_ffi.CallOutcomeInfo
import uniffi.threnody_ffi.Clip
import uniffi.threnody_ffi.FileFetch
import uniffi.threnody_ffi.FileInfo
import uniffi.threnody_ffi.FileOptions
import uniffi.threnody_ffi.GroupBanner
import uniffi.threnody_ffi.NoticeInfo
import uniffi.threnody_ffi.GroupInfo
import uniffi.threnody_ffi.GroupInvite
import uniffi.threnody_ffi.GroupMemberAction
import uniffi.threnody_ffi.GroupMemberRow
import uniffi.threnody_ffi.MemberRole
import uniffi.threnody_ffi.MentionInfo
import uniffi.threnody_ffi.NotifyLevel
import uniffi.threnody_ffi.QuoteInfo
import uniffi.threnody_ffi.ScheduledInfo
import uniffi.threnody_ffi.CardInfo
import uniffi.threnody_ffi.HistoryEntry
import uniffi.threnody_ffi.NodeEvent
import uniffi.threnody_ffi.ThrenodyNode
import uniffi.threnody_ffi.TickMark
import uniffi.threnody_ffi.TransferState

/**
 * One conversation, with a contact (extras [KEY] and [DEVICE]) or a group
 * ([GROUP]): its messages, a compose bar, and the conversation's settings.
 */
class ChatActivity : Activity() {
    private lateinit var node: ThrenodyNode
    /** The group id, for a group conversation. */
    private var group: String? = null
    /** Whose invitation to [group] was opened, when more than one person may invite us to it. */
    private var inviter: String? = null
    /** The invitation the banner shows, which Join and Decline answer. */
    private var shownInvite: GroupInvite? = null
    /** The anonymous identity this chat belongs to; null for the main one. */
    private var persona: String? = null
    /** They proved they are a contact of our main identity: its name. */
    private var revealedKnown: String? = null
    private var info: GroupInfo? = null
    private var key = ""
    /** The device to address; any of the account's devices reaches them all. */
    private var device = ""
    private var convo: Conversation? = null
    /** The voice and video call buttons, hidden in a group we're only invited to. */
    private var callButtons: List<View> = emptyList()
    private lateinit var bar: TopBar
    private lateinit var banner: LinearLayout
    /** The pinned messages, newest pin shown; tapping steps through them. */
    private lateinit var pinBar: LinearLayout
    private var pins: List<HistoryEntry> = emptyList()
    private var pinShown = 0
    /** In a group: whether we may pin (owner or admin). */
    private var mayPin = false
    /** The chat's shared theme, whether we keep our own look, and whether we may change it. */
    private var look: uniffi.threnody_ffi.ThemeInfo? = null
    private var ownLook = false
    private var mayTheme = true
    private lateinit var messages: LinearLayout
    private lateinit var scroll: ScrollView
    /** The chat is at its newest message, and stays there as it grows. */
    private var stick = true
    private lateinit var compose: EditText
    private lateinit var composeBar: LinearLayout
    /** "…" below the messages while the contact is typing. */
    private lateinit var typingBubble: View
    private val typingHandler = Handler(Looper.getMainLooper())
    /** We stopped typing for a while: say so. */
    private val stopTyping = Runnable { sendTyping(false) }
    /** The contact's "typing" went quiet (its "stopped" may be lost). */
    private val peerStopped = Runnable { showTyping(false) }
    /** Messages being sent, shown until history has them. */
    /** Messages being sent: their text, and what they quote. */
    private val pending = mutableListOf<Pair<String, QuoteInfo?>>()
    /** The message the next one replies to, shown above the compose bar. */
    private var replyTo: HistoryEntry? = null
    /** The part of [replyTo] the reply quotes (a selection), if not its start. */
    private var replyPart: String? = null
    private lateinit var replyBar: LinearLayout
    /** Offers, and holds, a card for a link being written. */
    private val previewBar by lazy { PreviewBar(this) { if (::node.isInitialized) node else null } }
    /** "🕓 2 scheduled · next Tue 08:00" above the compose bar; opens the list. */
    private lateinit var scheduledBar: TextView
    /** This chat's scheduled messages, soonest first. */
    private var scheduled: List<ScheduledInfo> = emptyList()
    /** This chat has a disappearing timer (it applies to scheduled messages when sent). */
    private var timerOn = false
    /** The messages on screen, so a quote can jump to what it quotes. */
    private var onScreen: List<HistoryEntry> = emptyList()
    private var unsubscribe: (() -> Unit)? = null
    private var myDevice = ""
    private val pickFileLauncher = ActivityResultRegistry.get(this)
    private val viewPhotoLauncher = ActivityResultRegistry.get(this)
    private val annotateLauncher = ActivityResultRegistry.get(this)
    private val clipLauncher = ActivityResultRegistry.get(this)
    /** Send (with text to send) or record a voice message (without). */
    private lateinit var sendButton: ImageButton
    private lateinit var micButton: ImageButton
    /** Over the message box while a voice message records. */
    private lateinit var recordingBar: LinearLayout
    private lateinit var recordingTime: TextView
    private lateinit var recordingHint: TextView
    private var voice: VoiceRecorder? = null
    /** Slid up: it goes on recording without a finger on the button. */
    private var voiceLocked = false
    /**
     * The press that slid up to lock is still down: lifting it keeps the
     * recording going, where lifting any other press stops it.
     */
    private var lockingPress = false
    /**
     * A voice message recorded and not sent yet (its plain file and length):
     * it waits in the message box, to play back, delete or send.
     */
    private var voiceDraft: Pair<java.io.File, Long>? = null
    /** In place of the message box while [voiceDraft] waits. */
    private lateinit var draftBar: LinearLayout
    private lateinit var draftPlay: ImageButton
    private lateinit var draftSeek: SeekBar
    private lateinit var draftTime: TextView
    /** Plays [voiceDraft] back; made when first played. */
    private var draftPlayer: android.media.MediaPlayer? = null
    private val draftTick = object : Runnable {
        override fun run() {
            val p = draftPlayer ?: return
            val (_, ms) = voiceDraft ?: return
            val length = if (p.duration > 0) p.duration else ms.toInt()
            if (length > 0) draftSeek.progress = (p.currentPosition * 1000L / length).toInt()
            draftTime.text = if (p.isPlaying || p.currentPosition > 0) Clips.clock(p.currentPosition.toLong()) else Clips.clock(ms)
            draftPlay.setImageResource(if (p.isPlaying) R.drawable.ic_pause else R.drawable.ic_play)
            draftPlay.contentDescription = if (p.isPlaying) "Pause" else "Play the voice message"
            if (p.isPlaying) typingHandler.postDelayed(this, 100)
        }
    }
    private var pressX = 0f
    private var pressY = 0f
    private val recordTick = object : Runnable {
        override fun run() {
            val v = voice ?: return
            recordingTime.text = "● " + Clips.clock(v.elapsed)
            typingHandler.postDelayed(this, 200)
        }
    }
    /** Plays voice messages, one at a time; made when first needed. */
    private var clipPlayer: ClipPlayer? = null
    /** Voice messages on screen by location: play button, bar, time. */
    private val voiceViews = mutableMapOf<String, Triple<ImageButton, SeekBar, TextView>>()
    /** Sensitive voice and video messages uncovered, by [mark]. */
    private val shownClips = mutableSetOf<String>()
    /** Incoming message ids already reported read. */
    private val sentRead = mutableSetOf<ULong>()
    /** On screen (between onStart and onStop): only then are messages read. */
    private var started = false
    /** Search in this chat: the box, its matches (newest first) and the one shown. */
    private lateinit var search: SearchBox
    private var hits: List<HistoryEntry> = emptyList()
    private var hit = -1
    private var terms: List<String> = emptyList()
    /** Bumped per search, so a slow one's results don't replace a newer one's. */
    private var searches = 0
    /** Each message's bubble and texts on screen, by [mark], to scroll to and highlight. */
    private val bubbles = mutableMapOf<String, View>()
    private val texts = mutableMapOf<String, MutableList<Pair<TextView, String>>>()
    /** Messages with mentions, by [mark]: their text as shown, and where the mentions are. */
    private val mentionRanges = mutableMapOf<String, Pair<String, List<Triple<Int, Int, Boolean>>>>()
    /** Links in bubbles' texts, by text. */
    private val linkCache = HashMap<String, List<uniffi.threnody_ffi.TextLink>>()
    /** A message to bring into view once loaded: its [mark] and time. */
    private var jump: Pair<String, ULong>? = null
    /**
     * The search result the chat was opened at: the first search selects
     * it. Kept apart from `jump`, which loading may finish with first.
     */
    private var openedAt: String? = null
    /** How many messages are loaded; grows to reach an older match. */
    @Volatile private var historyLimit = HISTORY_PAGE
    /** The oldest loaded message's time, and whether that is the whole history. */
    private var oldestLoaded = 0uL
    private var allLoaded = true

    /** Notes to self: a chat with our own account. */
    private var note = false
    /** The saved draft was put in the compose field (once per opening). */
    private var draftShown = false

    /** A history entry, or a message still being sent. */
    private data class Item(val entry: HistoryEntry, val sending: Boolean = false)

    /** The way back to a call running elsewhere. */
    private val callBanner by lazy { CallBanner(this) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        group = intent.getStringExtra(GROUP)
        inviter = intent.getStringExtra(INVITER)
        persona = intent.getStringExtra(PERSONA)
        note = intent.getBooleanExtra(NOTE, false)
        key = group ?: (if (note) NOTE else intent.getStringExtra(KEY)) ?: return finish()
        device = intent.getStringExtra(DEVICE) ?: key

        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        bar = TopBar(this) { finish() }.apply {
            title.text = if (group != null) "Group" else Threnody.short(device)
            // Builds without calls (the F-Droid build) have no call buttons.
            if (uniffi.threnody_ffi.callsBuiltIn()) {
                action(R.drawable.ic_videocam, if (group != null) "Group video call" else "Video call") { call(video = true) }
                action(R.drawable.ic_call, if (group != null) "Group call" else "Call") { call() }
                callButtons = (childCount - 2 until childCount).map { getChildAt(it) }
            }
            if (note) action(R.drawable.ic_search, "Search notes") { more(it) }
            else action(R.drawable.ic_more, if (group != null) "Group options" else "Contact options") { more(it) }
        }
        search = SearchBox(this, bar, "Search this chat", stepper = true, query = ::find, step = ::step) { endSearch() }
        // The chat's face beside its name; a tap on either shows its details.
        bar.avatar().show(bar.title.text.toString(), if (note) "notes" else key)
        bar.onTitle {
            when {
                group != null -> members()
                !note -> details()
            }
        }
        banner = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(12), dp(4), dp(12), 0)
            visibility = View.GONE
        }
        messages = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(12), dp(8), dp(12), dp(8))
        }
        // Typing indicator bubble (shown when peer is typing).
        typingBubble = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.START
            setPadding(0, dp(3), dp(48), dp(3))
            val bubble = LinearLayout(this@ChatActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                background = rounded(color(R.color.bubble_in), dp(16).toFloat())
                setPadding(dp(12), dp(8), dp(12), dp(8))
                addView(TextView(this@ChatActivity).apply {
                    text = "…"
                    textSize = 16f
                    setTextColor(color(R.color.muted))
                })
            }
            addView(bubble, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        typingBubble.visibility = View.GONE
        scroll = ScrollView(this).apply {
            isFillViewport = true
            addView(messages, MATCH_PARENT, WRAP_CONTENT)
        }
        composeBar = composeBar()
        // In groups, "@" offers the other members to mention.
        if (group != null) Mentions.attach(compose) {
            if (!::node.isInitialized) emptyList()
            else info?.members.orEmpty().filter { it != myDevice }.map { it to Threnody.nameOf(node, it) }
        }
        replyBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(12), dp(6), dp(4), 0)
            visibility = View.GONE
        }
        root.addView(bar, matchWrap)
        root.addView(callBanner.view, matchWrap)
        root.addView(banner, matchWrap)
        pinBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(16), dp(8), dp(16), dp(8))
            visibility = View.GONE
            background = ripple()
            setOnClickListener { nextPin() }
        }
        root.addView(pinBar, matchWrap)
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        scheduledBar = label("", 14f, R.color.accent).apply {
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(16), dp(8), dp(16), dp(8))
            visibility = View.GONE
            background = ripple()
            setOnClickListener { scheduledList() }
        }
        root.addView(scheduledBar, matchWrap)
        root.addView(replyBar, matchWrap)
        root.addView(previewBar.view, matchWrap)
        root.addView(composeBar, matchWrap)
        setContentView(root)
        fitSystemBars(root, bar, composeBar)
        // Keep the newest message in view when the keyboard opens (unless
        // a search match is).
        scroll.addOnLayoutChangeListener { _, _, _, _, b, _, _, _, ob -> if (b < ob && hit < 0) toBottom() }
        // Scrolling up lets go of the newest message; back down holds it again.
        scroll.setOnScrollChangeListener { _, _, _, _, _ -> stick = !scroll.canScrollVertically(1) }
        // Redraws and photos loading change the chat's height: follow it.
        messages.addOnLayoutChangeListener { _, _, t, _, b, _, ot, _, ob ->
            if (stick && hit < 0 && jump == null && b - t != ob - ot) scroll.post { scroll.scrollTo(0, messages.height) }
        }

        // Opened from a search result: that message, with the search open on it.
        val jumpAt = intent.getLongExtra(JUMP_AT_MS, 0L)
        if (jumpAt > 0) {
            jump = mark(jumpAt.toULong(), intent.getStringExtra(JUMP_DEVICE) ?: "") to jumpAt.toULong()
            openedAt = jump?.first
            intent.getStringExtra(JUMP_QUERY)?.takeIf { it.isNotBlank() }?.let { q ->
                search.open(typing = false)
                search.field.setText(q)
            }
        }

        Threading.background {
            node = try {
                Threnody.node(this, persona)
            } catch (e: Exception) {
                runOnUiThread {
                    Toast.makeText(this, e.message, Toast.LENGTH_LONG).show()
                    finish()
                }
                return@background
            }
            refresh()
            // Not connected: look for them across the internet now.
            if (group == null && !note) try { node.seek(device) } catch (e: Exception) {
                Threnody.say("! seek: ${e.message}")
            }
        }
    }

    private fun composeBar(): LinearLayout {
        compose = ComposeField(this, ::received).apply {
            hint = "Message"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 6)
            // The pill around it is the field; the text sits in it bare.
            background = null
            textSize = 16f
            setPadding(dp(4), dp(12), dp(4), dp(12))
            setTextColor(color(R.color.text))
            setHintTextColor(color(R.color.muted))
            addTextChangedListener(object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                override fun afterTextChanged(s: Editable?) {
                    typingHandler.removeCallbacks(stopTyping)
                    previewBar.onText(s.toString())
                    if (::sendButton.isInitialized) {
                        // A microphone until there's text to send.
                        val blank = s.toString().isBlank()
                        sendButton.visibility = if (blank) View.GONE else View.VISIBLE
                        micButton.visibility = if (blank) View.VISIBLE else View.GONE
                    }
                    if (s.toString().isBlank()) return sendTyping(false)
                    sendTyping(true)
                    typingHandler.postDelayed(stopTyping, TYPING_IDLE_MS)
                }
            })
        }
        fun button(res: Int, label: String, tint: Int, onClick: (View) -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(color(tint))
            contentDescription = label
            tooltipText = label
            background = ripple(borderless = true)
            layoutParams = LinearLayout.LayoutParams(dp(Design.touchTargetMin), dp(Design.touchTargetMin))
            setOnClickListener { v -> Design.mediumHaptic(v); onClick(v) }
        }
        // One rounded field holding emoji, the text and attach; beside it a
        // round button that records voice, or sends once there's text.
        val pill = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.BOTTOM
            background = rounded(color(R.color.field), dp(26).toFloat())
            setPadding(dp(2), 0, dp(2), 0)
            addView(button(R.drawable.ic_emoji, "Emoji", R.color.muted) {
                EmojiPicker(this@ChatActivity, "Emoji", stay = true) { e ->
                    val at = compose.selectionStart.coerceAtLeast(0)
                    compose.text.replace(at, compose.selectionEnd.coerceAtLeast(at), e)
                }.show()
            }, LinearLayout.LayoutParams(dp(46), dp(50)))
            addView(compose, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(button(R.drawable.ic_attach, "Send photos, files and more", R.color.muted) { attach(it) },
                LinearLayout.LayoutParams(dp(46), dp(50)))
        }
        fun round(res: Int, label: String, onClick: (View) -> Unit) = button(res, label, R.color.on_accent, onClick).apply {
            background = android.graphics.drawable.RippleDrawable(
                android.content.res.ColorStateList.valueOf(withAlpha(color(R.color.on_accent), 0x40)),
                android.graphics.drawable.GradientDrawable().apply {
                    shape = android.graphics.drawable.GradientDrawable.OVAL
                    setColor(color(R.color.accent))
                }, null,
            )
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.BOTTOM
            setBackgroundColor(color(R.color.bar))
            setPadding(dp(Design.sm), dp(6), dp(Design.sm), dp(Design.sm))
            recordingTime = label("● 0:00", 16f, R.color.warning).apply { setTypeface(typeface, Typeface.BOLD) }
            recordingHint = label("‹ Slide to cancel · slide up to lock", 13f, R.color.muted).apply {
                gravity = Gravity.END
            }
            recordingBar = LinearLayout(this@ChatActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                visibility = View.GONE
                background = rounded(color(R.color.field), dp(26).toFloat())
                setPadding(dp(4), 0, dp(14), 0)
                addView(button(R.drawable.ic_delete, "Cancel the voice message", R.color.muted) { cancelVoice() },
                    LinearLayout.LayoutParams(dp(44), dp(44)))
                addView(recordingTime, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
                addView(recordingHint, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = dp(8) })
            }
            draftPlay = button(R.drawable.ic_play, "Play the voice message", R.color.accent) { toggleDraftPlayback() }
            draftSeek = SeekBar(this@ChatActivity).apply {
                max = 1000
                contentDescription = "Voice message position"
                setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                    override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) {
                        val p = draftPlayer ?: return
                        if (fromUser && p.duration > 0) {
                            p.seekTo((p.duration.toLong() * progress / 1000).toInt())
                            draftTime.text = Clips.clock(p.currentPosition.toLong())
                        }
                    }
                    override fun onStartTrackingTouch(bar: SeekBar) {}
                    override fun onStopTrackingTouch(bar: SeekBar) {}
                })
            }
            draftTime = label("0:00", 14f, R.color.muted)
            draftBar = LinearLayout(this@ChatActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                visibility = View.GONE
                background = rounded(color(R.color.field), dp(26).toFloat())
                setPadding(dp(4), 0, dp(14), 0)
                addView(button(R.drawable.ic_delete, "Delete the voice message", R.color.muted) { discardVoiceDraft() },
                    LinearLayout.LayoutParams(dp(44), dp(44)))
                addView(draftPlay, LinearLayout.LayoutParams(dp(44), dp(44)))
                addView(draftSeek, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                addView(draftTime, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            }
            val field = android.widget.FrameLayout(this@ChatActivity).apply {
                addView(pill, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(recordingBar, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
                addView(draftBar, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            }
            addView(field, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginEnd = dp(Design.sm) })
            sendButton = round(R.drawable.ic_send, "Send") { send() }.apply { visibility = View.GONE }
            if (!note) {
                // Holding Send offers to send later; the press doesn't send.
                sendButton.contentDescription = "Send (hold to send later)"
                sendButton.tooltipText = "Send (hold to send later)"
                sendButton.setOnLongClickListener { v -> Design.mediumHaptic(v); laterMenu(v); true }
            }
            addView(sendButton, LinearLayout.LayoutParams(dp(50), dp(50)))
            micButton = round(R.drawable.ic_mic, "Hold to record a voice message") {}
            micButton.setOnTouchListener { v, ev -> voiceTouch(v, ev) }
            addView(micButton, LinearLayout.LayoutParams(dp(50), dp(50)))
        }
    }

    // ----- Voice and video messages -----

    /**
     * Hold the microphone to record; let go and the recording waits in the
     * message box, to play back, delete or send. Sliding left cancels;
     * sliding up locks it on, so the finger can lift and recording goes on
     * until the stop button. Nothing is sent until Send is tapped.
     */
    private fun voiceTouch(v: View, ev: MotionEvent): Boolean {
        fun inside() = ev.x >= 0 && ev.y >= 0 && ev.x <= v.width && ev.y <= v.height
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                // A tap on Send (a draft) or Stop (locked): acted on when lifted.
                if (voiceDraft != null || (voice != null && voiceLocked)) {
                    v.isPressed = true
                    return true
                }
                pressX = ev.rawX
                pressY = ev.rawY
                lockingPress = false
                // The slide is ours: the chat behind mustn't take it as a scroll.
                v.parent?.requestDisallowInterceptTouchEvent(true)
                startVoice(v)
            }
            MotionEvent.ACTION_MOVE -> if (voice != null && !voiceLocked) {
                if (pressX - ev.rawX > dp(120)) cancelVoice()
                else if (pressY - ev.rawY > dp(80)) {
                    lockingPress = true
                    lockVoice(v)
                }
            }
            MotionEvent.ACTION_UP -> {
                v.isPressed = false
                if (voiceDraft != null) {
                    if (inside()) {
                        Design.mediumHaptic(v)
                        sendVoiceDraft()
                    }
                    return true
                }
                val rec = voice ?: return true
                when {
                    // The finger that locked it lifts: recording goes on.
                    voiceLocked && lockingPress -> lockingPress = false
                    voiceLocked -> if (inside()) stopVoice()
                    rec.elapsed >= Clips.MIN_MS -> stopVoice()
                    else -> {
                        cancelVoice()
                        Toast.makeText(this, "Hold to record a voice message", Toast.LENGTH_SHORT).show()
                    }
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                v.isPressed = false
                // Only a held recording ends with the press; a locked one goes on.
                if (voice != null && !voiceLocked) cancelVoice()
                lockingPress = false
            }
        }
        return true
    }

    private fun startVoice(v: View) {
        if (!::node.isInitialized || voice != null || voiceDraft != null) return
        if (checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) != android.content.pm.PackageManager.PERMISSION_GRANTED) {
            return requestPermissions(arrayOf(android.Manifest.permission.RECORD_AUDIO), VOICE_PERMISSION)
        }
        clipPlayer?.stop()
        voice = try {
            // At the longest a voice message may be, it stops into a draft.
            VoiceRecorder(this, Clips.file(this, persona, video = false)) { runOnUiThread { stopVoice() } }
        } catch (e: Exception) {
            Toast.makeText(this, "Couldn't record: ${e.message}", Toast.LENGTH_LONG).show()
            return
        }
        Design.lightHaptic(v)
        voiceLocked = false
        recordingHint.text = "‹ Slide to cancel · slide up to lock"
        recordingBar.visibility = View.VISIBLE
        compose.visibility = View.INVISIBLE
        typingHandler.post(recordTick)
    }

    private fun lockVoice(v: View) {
        voiceLocked = true
        Design.mediumHaptic(v)
        recordingHint.text = "Locked · tap stop to review"
        micButton.setImageResource(R.drawable.ic_stop)
        micButton.contentDescription = "Stop recording"
        micButton.tooltipText = "Stop recording"
    }

    private fun endVoice() {
        typingHandler.removeCallbacks(recordTick)
        voice = null
        voiceLocked = false
        lockingPress = false
        recordingBar.visibility = View.GONE
        compose.visibility = View.VISIBLE
        micButton.setImageResource(R.drawable.ic_mic)
        micButton.contentDescription = "Hold to record a voice message"
        micButton.tooltipText = "Hold to record a voice message"
    }

    private fun cancelVoice() {
        val rec = voice ?: return
        endVoice()
        rec.cancel()
    }

    /** Stops recording; what was recorded waits in the message box as a draft. */
    private fun stopVoice() {
        val rec = voice ?: return
        endVoice()
        val (file, ms) = rec.stop() ?: return
        showVoiceDraft(file, ms)
    }

    private fun showVoiceDraft(file: java.io.File, ms: Long) {
        voiceDraft = file to ms
        draftSeek.progress = 0
        draftTime.text = Clips.clock(ms)
        draftPlay.setImageResource(R.drawable.ic_play)
        draftBar.visibility = View.VISIBLE
        compose.visibility = View.INVISIBLE
        micButton.setImageResource(R.drawable.ic_send)
        micButton.contentDescription = "Send the voice message"
        micButton.tooltipText = "Send the voice message"
    }

    /** Takes the draft out of the message box; its file is the caller's. */
    private fun clearVoiceDraft(): Pair<java.io.File, Long>? {
        val draft = voiceDraft ?: return null
        voiceDraft = null
        typingHandler.removeCallbacks(draftTick)
        draftPlayer?.release()
        draftPlayer = null
        draftBar.visibility = View.GONE
        compose.visibility = View.VISIBLE
        micButton.setImageResource(R.drawable.ic_mic)
        micButton.contentDescription = "Hold to record a voice message"
        micButton.tooltipText = "Hold to record a voice message"
        return draft
    }

    private fun discardVoiceDraft() {
        val (file, _) = clearVoiceDraft() ?: return
        Threading.io { Media.dropPlain(file) }
    }

    private fun sendVoiceDraft() {
        val (file, ms) = clearVoiceDraft() ?: return
        sendClip(file, ms, video = false)
    }

    private fun pauseVoiceDraft() {
        draftPlayer?.takeIf { it.isPlaying }?.pause()
        typingHandler.post(draftTick)
    }

    /** Plays the draft back from where it was, or pauses it. */
    private fun toggleDraftPlayback() {
        val (file, _) = voiceDraft ?: return
        val p = draftPlayer ?: try {
            android.media.MediaPlayer().apply {
                setAudioAttributes(
                    android.media.AudioAttributes.Builder()
                        .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                        .setContentType(android.media.AudioAttributes.CONTENT_TYPE_SPEECH)
                        .build()
                )
                setDataSource(file.path)
                setOnCompletionListener {
                    seekTo(0)
                    typingHandler.post(draftTick)
                }
                prepare()
            }.also { draftPlayer = it }
        } catch (e: Exception) {
            Toast.makeText(this, "Couldn't play it: ${e.message}", Toast.LENGTH_SHORT).show()
            return
        }
        if (p.isPlaying) p.pause() else {
            clipPlayer?.stop()
            p.start()
        }
        typingHandler.post(draftTick)
    }

    /** Records a video message on a screen of its own, then sends it. */
    private fun recordVideo() {
        if (!::node.isInitialized) return
        val need = arrayOf(android.Manifest.permission.CAMERA, android.Manifest.permission.RECORD_AUDIO)
            .filter { checkSelfPermission(it) != android.content.pm.PackageManager.PERMISSION_GRANTED }
        if (need.isNotEmpty()) return requestPermissions(need.toTypedArray(), VIDEO_PERMISSION)
        clipPlayer?.stop()
        clipLauncher.launch(
            Intent(this, VideoClipActivity::class.java)
                .putExtra(VideoClipActivity.PERSONA, persona)
                .putExtra(VideoClipActivity.MAX_BYTES, node.maxFileSize().toLong())
        ) { resultCode, data ->
            val path = data?.getStringExtra(VideoClipActivity.RESULT_PATH)
            if (resultCode != RESULT_OK || path == null) return@launch
            sendClip(java.io.File(path), data.getLongExtra(VideoClipActivity.RESULT_DURATION_MS, 0), video = true)
        }
    }

    /**
     * Sends a recorded voice or video message, kept sealed with the chat's
     * files; the plain recording is deleted either way.
     */
    private fun sendClip(file: java.io.File, measured: Long, video: Boolean) {
        Threnody.touch()
        Threading.background {
            var location: String? = null
            // The file's own length when it says, else as timed.
            val ms = try {
                android.media.MediaMetadataRetriever().use {
                    it.setDataSource(file.path)
                    it.extractMetadata(android.media.MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull()
                }
            } catch (_: Exception) {
                null
            } ?: measured
            val error = try {
                val data = file.readBytes()
                if (data.size.toULong() > node.maxFileSize()) {
                    "it's larger than " + Formatter.formatShortFileSize(this, node.maxFileSize().toLong())
                } else {
                    val options = FileOptions(false, "", 0uL, Clip(video, ms.coerceIn(0, Int.MAX_VALUE.toLong()).toUInt()))
                    val kept = Media.savePrivate(this, file.name, data, persona)
                        ?: throw IllegalStateException("couldn't keep it")
                    location = kept
                    sendFileTo(file.name, data, kept, options)
                    null
                }
            } catch (e: Exception) {
                e.message ?: "error"
            } finally {
                Media.dropPlain(file)
            }
            if (error != null) {
                Media.forget(this, location)
                runOnUiThread { Toast.makeText(this, "Couldn't send: $error", Toast.LENGTH_LONG).show() }
            }
            refresh()
        }
    }

    private fun player(): ClipPlayer = clipPlayer ?: ClipPlayer(this).also { p ->
        clipPlayer = p
        p.listener = object : ClipPlayer.Listener {
            override fun progress(location: String, positionMs: Int, durationMs: Int, playing: Boolean) {
                val (button, bar, time) = voiceViews[location] ?: return
                button.setImageResource(if (playing) R.drawable.ic_pause else R.drawable.ic_play)
                button.contentDescription = if (playing) "Pause" else "Play"
                if (durationMs > 0) bar.progress = (positionMs * 1000L / durationMs).toInt()
                else bar.progress = 0
                if (playing || positionMs > 0) time.text = Clips.clock(positionMs.toLong())
                else time.tag?.let { time.text = it as String }
            }
        }
    }

    /**
     * A voice message (play button, bar, length) or a video message (a
     * still to tap). A sensitive one arrives covered.
     */
    private fun clipView(f: uniffi.threnody_ffi.FileInfo, clip: Clip, e: HistoryEntry, fg: Int): View {
        val location = f.location
        val what = if (clip.video) "video message" else "voice message"
        val length = if (clip.durationMs > 0u) Clips.clock(clip.durationMs.toLong()) else ""
        if (location == null || (f.sensitive && !e.outgoing && mark(e) !in shownClips)) {
            val head = what.replaceFirstChar { it.uppercase() }
            val cover = label(
                if (location == null) "$head\n${missingText(f, e) ?: "not on this device"}"
                else "Sensitive $what\nTap to play",
                14f, R.color.on_cover,
            ).apply {
                icon(when {
                    location != null -> R.drawable.ic_lock
                    clip.video -> R.drawable.ic_videocam
                    else -> R.drawable.ic_mic
                }, 28, top = true)
                gravity = Gravity.CENTER
                setTypeface(typeface, Typeface.BOLD)
                background = rounded(color(R.color.cover), dp(12).toFloat()).apply { setStroke(dp(1), color(R.color.cover_edge)) }
                setPadding(dp(16), dp(16), dp(16), dp(16))
                minWidth = dp(200)
                contentDescription = if (location == null) "$what: ${missingText(f, e) ?: "not available"}" else "Sensitive $what, tap to uncover"
            }
            if (location == null) fetchable(cover, f, e) { "$head\n$it" }
            if (location != null) cover.setOnClickListener {
                shownClips.add(mark(e))
                val parent = cover.parent as? ViewGroup ?: return@setOnClickListener
                val at = parent.indexOfChild(cover)
                parent.removeViewAt(at)
                parent.addView(clipView(f, clip, e, fg), at)
            }
            return cover
        }
        if (clip.video) {
            val side = minOf(dp(220), resources.displayMetrics.widthPixels - dp(140))
            val still = ImageView(this).apply {
                scaleType = ImageView.ScaleType.CENTER_CROP
                background = rounded(android.graphics.Color.BLACK, dp(12).toFloat())
                clipToOutline = true
            }
            Clips.still(this, location, side) { b -> runOnUiThread { still.setImageBitmap(b) } }
            val play = ImageView(this).apply {
                setImageResource(R.drawable.ic_play)
                imageTintList = android.content.res.ColorStateList.valueOf(android.graphics.Color.WHITE)
                background = android.graphics.drawable.GradientDrawable().apply {
                    shape = android.graphics.drawable.GradientDrawable.OVAL
                    setColor(android.graphics.Color.argb(140, 0, 0, 0))
                }
                setPadding(dp(10), dp(10), dp(10), dp(10))
            }
            return android.widget.FrameLayout(this).apply {
                addView(still, android.widget.FrameLayout.LayoutParams(side, side * 4 / 3))
                addView(play, android.widget.FrameLayout.LayoutParams(dp(52), dp(52), Gravity.CENTER))
                if (length.isNotEmpty()) addView(label(length, 12f, R.color.on_cover).apply {
                    setTextColor(android.graphics.Color.WHITE)
                    setShadowLayer(4f, 0f, 0f, android.graphics.Color.BLACK)
                    setPadding(dp(8), dp(4), dp(8), dp(6))
                }, android.widget.FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.BOTTOM or Gravity.END))
                contentDescription = "Video message" + if (length.isNotEmpty()) ", $length" else ""
                // On the still and button too: the bubble's long press makes them take touches.
                val open = View.OnClickListener { playVideo(location) }
                setOnClickListener(open)
                still.setOnClickListener(open)
                play.setOnClickListener(open)
            }
        }
        val tint = android.content.res.ColorStateList.valueOf(fg)
        val button = ImageButton(this).apply {
            setImageResource(R.drawable.ic_play)
            imageTintList = tint
            background = ripple(borderless = true)
            contentDescription = "Play"
            setOnClickListener { player().toggle(location) }
        }
        val bar = SeekBar(this).apply {
            max = 1000
            progressTintList = tint
            thumbTintList = tint
            progressBackgroundTintList = tint
            contentDescription = "Voice message position"
            setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                override fun onProgressChanged(s: SeekBar, p: Int, fromUser: Boolean) {
                    if (fromUser) clipPlayer?.seek(location, p / 1000f)
                }
                override fun onStartTrackingTouch(s: SeekBar) {}
                override fun onStopTrackingTouch(s: SeekBar) {}
            })
        }
        val time = label(length.ifEmpty { "0:00" }, 12f, R.color.text).apply {
            setTextColor(fg)
            tag = length.ifEmpty { "0:00" }
        }
        voiceViews[location] = Triple(button, bar, time)
        // Playing on from before a reload: show where it is.
        clipPlayer?.position(location)?.let { (pos, len) ->
            if (len > 0) bar.progress = (pos * 1000L / len).toInt()
            time.text = Clips.clock(pos.toLong())
            if (clipPlayer?.playing == true) button.setImageResource(R.drawable.ic_pause)
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(button, LinearLayout.LayoutParams(dp(44), dp(44)))
            addView(bar, LinearLayout.LayoutParams(dp(160), WRAP_CONTENT))
            addView(time, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
    }

    /** Plays a video message full screen, inside the app. */
    private fun playVideo(location: String) {
        clipPlayer?.stop()
        val dialog = android.app.Dialog(this, android.R.style.Theme_Black_NoTitleBar_Fullscreen)
        val video = android.widget.VideoView(this)
        val box = android.widget.FrameLayout(this).apply {
            setBackgroundColor(android.graphics.Color.BLACK)
            addView(video, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.CENTER))
            setOnClickListener { dialog.dismiss() }
        }
        video.setMediaController(android.widget.MediaController(this).apply { setAnchorView(box) })
        video.setOnPreparedListener { video.start() }
        video.setOnErrorListener { _, _, _ ->
            Toast.makeText(this, "Couldn't play this video", Toast.LENGTH_SHORT).show()
            dialog.dismiss()
            true
        }
        // VideoView plays files: a sealed one from a decrypted copy in the
        // cache, deleted when the player closes.
        var plain: java.io.File? = null
        var closed = false
        dialog.setContentView(box)
        dialog.setOnDismissListener {
            video.stopPlayback()
            closed = true
            plain?.let { f -> Threading.background { Media.dropPlain(f) } }
        }
        Privacy.secure(dialog)
        dialog.show()
        Threading.background {
            val copy = try {
                Media.plainCopy(this, location)
            } catch (_: Exception) {
                runOnUiThread {
                    Toast.makeText(this, "Couldn't play this video", Toast.LENGTH_SHORT).show()
                    dialog.dismiss()
                }
                return@background
            }
            runOnUiThread {
                if (closed) {
                    copy?.let { f -> Threading.background { Media.dropPlain(f) } }
                } else {
                    plain = copy
                    if (copy != null) video.setVideoPath(copy.path) else video.setVideoURI(Uri.parse(location))
                }
            }
        }
    }

    /** Whether the contact was last told we are typing, and when. */
    private var typingSent = false
    private var typingSentAt = 0L

    /**
     * Tells the contact (not a group) we started or stopped typing: on a
     * change, and again now and then while typing goes on.
     */
    private fun sendTyping(active: Boolean) {
        if (group != null || note || !::node.isInitialized) return
        val now = android.os.SystemClock.elapsedRealtime()
        if (typingSent == active && !(active && now - typingSentAt > TYPING_AGAIN_MS)) return
        if (active && !Privacy.sendTyping(this)) return
        typingSent = active
        typingSentAt = now
        val d = device
        Threading.background { try { node.setTyping(d, active) } catch (e: Exception) { Threnody.say("! setTyping: ${e.message}") } }
    }

    /** Shows or hides "…" for the contact typing. */
    private fun showTyping(active: Boolean) {
        if (group != null) return
        typingHandler.removeCallbacks(peerStopped)
        if (active) typingHandler.postDelayed(peerStopped, PEER_TYPING_MS)
        val was = typingBubble.visibility == View.VISIBLE
        typingBubble.visibility = if (active) View.VISIBLE else View.GONE
        if (active && !was && !scroll.canScrollVertically(1)) toBottom()
    }

    override fun onStart() {
        super.onStart()
        started = true
        callBanner.start()
        Threnody.visible++
        Threnody.visibleChat = setOf(key, device) + (convo?.devices ?: emptyList())
        ThrenodyService.clearNotification(this, ThrenodyService.notificationKey(key, persona))
        unsubscribe = Threnody.subscribe { e ->
            if (!concerns(e)) return@subscribe
            if (e is NodeEvent.Typing) {
                runOnUiThread { showTyping(e.active) }
                return@subscribe
            }
            // A download's progress: its label, not a reload.
            if (e is NodeEvent.TransferChanged && e.state !is TransferState.Done) {
                runOnUiThread { showOffer(e.id, e.state) }
                return@subscribe
            }
            // A message from them ends their typing.
            if (e is NodeEvent.Message) runOnUiThread { showTyping(false) }
            when (e) {
                is NodeEvent.CredentialPresented -> runOnUiThread {
                    CredentialUi.showProof(this, node, e.peer, e.issuer, e.schema, e.attributes, e.pseudonym)
                }
                is NodeEvent.CredentialReceived -> runOnUiThread {
                    Toast.makeText(this, "Got a credential (${e.schema}). Menu → Credentials lists it.", Toast.LENGTH_LONG).show()
                }
                is NodeEvent.CredentialFailed -> runOnUiThread {
                    Toast.makeText(this, "Credential: ${e.reason}", Toast.LENGTH_LONG).show()
                }
                is NodeEvent.GroupMembersChanged -> memberNotice(e)?.let { text ->
                    runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
                }
                is NodeEvent.GroupRoleChanged -> roleNotice(e)?.let { text ->
                    runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
                }
                else -> {}
            }
            refreshSoon()
        }
        if (::node.isInitialized) refreshSoon()
    }

    override fun onResume() {
        super.onResume()
        // Exact alarms may have been allowed meanwhile: set the alarm again.
        Threading.background { Scheduled.arm(applicationContext) }
    }

    override fun onStop() {
        started = false
        callBanner.stop()
        saveDraft()
        // Leaving the chat stops a voice message playing. A recording stops
        // too, and waits as a draft (a press too short to be one is dropped).
        clipPlayer?.stop()
        pauseVoiceDraft()
        voice?.let { if (voiceLocked || it.elapsed >= Clips.MIN_MS) stopVoice() else cancelVoice() }
        typingHandler.removeCallbacks(stopTyping)
        sendTyping(false)
        Threnody.visible--
        Threnody.visibleChat = emptySet()
        unsubscribe?.invoke()
        super.onStop()
    }

    /** Whether an event is about this conversation (cheap check, then by account). */
    private fun concerns(e: NodeEvent): Boolean {
        if (e is NodeEvent.HistorySynced) return true
        group?.let { g ->
            return when (e) {
                is NodeEvent.GroupMessage -> e.group == g
                is NodeEvent.GroupFile -> e.group == g
                is NodeEvent.GroupMembersChanged -> e.group == g
                is NodeEvent.GroupRoleChanged -> e.group == g
                is NodeEvent.GroupChangeFailed -> e.group == g
                is NodeEvent.GroupOutOfStep -> e.group == g
                is NodeEvent.GroupUpgradePending -> e.group == g
                is NodeEvent.GroupJoined -> e.group == g
                is NodeEvent.GroupLeft -> e.group == g
                is NodeEvent.Delivered -> e.group == g
                is NodeEvent.Reacted -> e.group == g
                is NodeEvent.Pinned -> e.group == g
                is NodeEvent.ScheduledChanged -> e.group == g
                is NodeEvent.ScheduledFailed -> e.group == g
                else -> false
            }
        }
        val peer = when (e) {
            is NodeEvent.Message -> e.peer
            is NodeEvent.MessageRequest -> e.peer
            is NodeEvent.MessagesDeleted -> e.peer
            is NodeEvent.MessageEdited -> e.peer
            is NodeEvent.File -> e.peer
            is NodeEvent.FileOffered -> e.peer
            is NodeEvent.TransferChanged -> e.peer
            is NodeEvent.Connected -> e.peer
            is NodeEvent.Disconnected -> e.peer
            is NodeEvent.ApprovalChanged -> e.peer
            is NodeEvent.Delivered -> e.peer
            is NodeEvent.ProfileChanged -> e.peer
            is NodeEvent.Reacted -> if (e.group == null) e.peer else return false
            is NodeEvent.Pinned -> if (e.group == null) e.peer else return false
            is NodeEvent.IdentityRevealed -> e.peer
            is NodeEvent.Typing -> e.peer
            is NodeEvent.Fallback -> e.peer
            is NodeEvent.Read -> e.peer
            is NodeEvent.CredentialOffered -> e.offer.peer
            is NodeEvent.CredentialAsked -> e.ask.peer
            is NodeEvent.CredentialReceived -> e.peer
            is NodeEvent.CredentialPresented -> e.peer
            is NodeEvent.CredentialFailed -> e.peer
            is NodeEvent.ScheduledChanged -> e.peer ?: return false
            is NodeEvent.ScheduledFailed -> e.peer ?: return false
            is NodeEvent.AccountChanged -> return true
            else -> return false
        }
        // A device we haven't seen yet may belong to this account.
        return peer == device || convo?.devices?.contains(peer) == true ||
            (::node.isInitialized && Threnody.key(node.contacts(), peer) == key)
    }

    /** Reloads the conversation's state and messages (on the worker thread). */
    /** A refresh is queued and hasn't started yet. */
    private val refreshQueued = java.util.concurrent.atomic.AtomicBoolean(false)
    /** Counts refreshes, so only the newest one is drawn. */
    private val refreshGen = java.util.concurrent.atomic.AtomicInteger(0)

    /**
     * Reloads on the worker thread, once for however many asks arrive
     * meanwhile: a burst of events (a backlog of messages arriving) would
     * otherwise redraw everything once per message.
     */
    private fun refreshSoon() {
        if (refreshQueued.compareAndSet(false, true)) Threading.background {
            refreshQueued.set(false)
            refresh()
        }
    }

    private fun refresh() {
        val gen = refreshGen.incrementAndGet()
        val g = group
        val history: List<HistoryEntry>
        var c: Conversation? = null
        var gi: GroupInfo? = null
        val limit = historyLimit
        if (note) {
            history = try { node.noteHistory(limit) } catch (_: Exception) { emptyList() }
        } else if (g != null) {
            gi = node.groups().firstOrNull { it.id == g }
            history = try { node.groupHistory(g, limit) } catch (_: Exception) { emptyList() }
        } else {
            // The key moves from device to account once the account is known.
            c = Threnody.conversations(node, persona).firstOrNull { it.key == key || device in it.devices }
            if (c != null) {
                key = c.key
                device = c.device
            }
            history = try { node.history(device, limit) } catch (_: Exception) { emptyList() }
        }
        revealedKnown = c?.revealed?.let { fp ->
            // Known to the main identity, which is who they revealed themselves to.
            Threnody.start(this).contacts().firstOrNull { it.fingerprint == fp }?.let { it.name ?: Threnody.short(fp) }
        }
        val me = node.deviceFingerprint()
        myDevice = me
        // On screen: what's here is read now.
        if (started) try { node.markSeen(if (note) "notes" else g ?: key) } catch (_: Exception) {}
        val items = history.map { Item(it) } + synchronized(pending) {
            pending.map { (text, quote) -> Item(HistoryEntry(atMs = ULong.MAX_VALUE, outgoing = true, device = me, text = text, disappearing = false, file = null, delivered = false, read = false, id = 0uL, edited = false, recipients = 0u, deliveredTo = 0u, reactions = emptyList(), quote = quote, mentions = emptyList(), poll = null, forwarded = false, list = null, tick = TickMark.None), sending = true) }
        }
        val names = if (g != null) history.map { it.device }.distinct().associateWith { Threnody.nameOf(node, it) } else emptyMap()
        val pinned = try {
            when {
                note -> node.pinnedNotes()
                g != null -> node.groupPinned(g)
                else -> node.pinned(device)
            }
        } catch (_: Exception) { emptyList() }
        val draft = if (!draftShown) try {
            when {
                note -> node.noteDraft()
                g != null -> node.groupDraft(g)
                else -> node.draft(device)
            }
        } catch (_: Exception) { "" } else ""
        val canPin = g == null || try { node.mayPinInGroup(g) } catch (_: Exception) { false }
        val theme = try {
            when {
                note -> node.noteTheme()
                g != null -> node.groupTheme(g)
                else -> node.chatTheme(device)
            }
        } catch (_: Exception) { null }
        val own = !note && try { if (g != null) node.groupOwnLook(g) else node.ownLook(device) } catch (_: Exception) { false }
        val canTheme = g == null || try { node.mayThemeGroup(g) } catch (_: Exception) { false }
        val later = try {
            when {
                note -> emptyList()
                g != null -> node.groupScheduled(g)
                else -> node.scheduled(device)
            }
        } catch (_: Exception) { emptyList() }
        val timer = !note && try {
            (if (g != null) node.groupDisappearing(g) else node.disappearing(device)) != null
        } catch (_: Exception) { false }
        // Credential offers and requests from this contact, waiting for an answer.
        val pending = if (g == null && !note) try { node.pendingCredentials(device) } catch (_: Exception) { null } else null
        val offers = pending?.offers.orEmpty()
        val asks = pending?.asks.orEmpty()
        runOnUiThread {
            // A newer refresh will draw instead.
            if (gen != refreshGen.get()) return@runOnUiThread
            convo = c
            info = gi
            mayPin = canPin
            mayTheme = canTheme
            if (theme != look || own != ownLook) {
                look = theme
                ownLook = own
                applyLook()
            }
            showPins(pinned)
            timerOn = timer
            showScheduled(later)
            // What we left unsent here last time.
            if (!draftShown) {
                draftShown = true
                if (draft.isNotEmpty() && compose.text.isEmpty()) {
                    compose.setText(draft)
                    compose.setSelection(compose.text.length)
                }
            }
            if (Threnody.visibleChat.isNotEmpty()) {
                Threnody.visibleChat = setOf(key, device) + (c?.devices ?: emptyList())
            }
            if (note) noteHeader() else if (g != null) groupHeader(gi) else header(c)
            for (o in offers) banner(
                "${c?.title ?: "They"} offers you a credential (${o.schema}).",
                "Review" to { CredentialUi.answerOffer(this, node, o) { refreshSoon() } },
            )
            for (q in asks) banner(
                "${c?.title ?: "They"} asks you to prove " +
                    (if (q.keys.isEmpty()) "you hold a credential (${q.schema})." else "${q.keys.joinToString(", ")} (${q.schema})."),
                "Review" to { CredentialUi.answerAsk(this, node, q) { refreshSoon() } },
            )
            oldestLoaded = history.firstOrNull()?.atMs ?: 0uL
            allLoaded = history.size.toUInt() < limit
            show(items, names)
            jump?.let { (k, at) ->
                when {
                    k in bubbles -> { jump = null; reveal(k) }
                    // Older than what's loaded: load back to it, then look again.
                    !allLoaded && oldestLoaded > at && historyLimit < MAX_HISTORY -> loadUntil(at)
                    else -> jump = null
                }
            }
            // Incoming messages now on screen are read (1:1 chats only).
            if (g == null && !note && started && Privacy.sendReadReceipts(this)) {
                val ids = synchronized(sentRead) {
                    items.filter { !it.entry.outgoing && it.entry.id != 0uL && it.entry.id !in sentRead }.map { it.entry.id }
                }
                if (ids.isNotEmpty()) Threading.background {
                    // Without a session now, they're reported on a later refresh.
                    try {
                        node.reportRead(device, ids)
                        synchronized(sentRead) { sentRead.addAll(ids) }
                    } catch (e: Exception) {
                        Threnody.say("! reportRead: ${e.message}")
                    }
                }
            }
        }
    }

    private fun noteHeader() {
        bar.title.text = "Note to self"
        // No one to call in our own notes.
        callButtons.forEach { it.visibility = View.GONE }
        bar.face?.show("Note to self", "notes")
        bar.subtitle.visibility = View.VISIBLE
        bar.subtitle.text = "On all your devices"
        bar.subtitle.setTextColor(color(R.color.muted))
        clearBanner()
        composeBar.visibility = View.VISIBLE
    }

    private fun header(c: Conversation?) {
        bar.title.text = c?.title ?: Threnody.short(device)
        bar.face?.show(bar.title.text.toString(), key)
        bar.subtitle.visibility = View.VISIBLE
        // Short, in a quiet colour; only "Not verified" stands out (the banner says what to do).
        val parts = listOfNotNull(
            persona?.let { "🎭 as ${Threnody.personaLabels[it] ?: "anonymous"}" },
            if (c?.connected == true) "Online" else "Offline",
            c?.devices?.size?.takeIf { it > 1 }?.let { "$it devices" },
            if (c?.verified == true) "Verified" else "Not verified",
        )
        val line = android.text.SpannableString(parts.joinToString(" · "))
        if (c?.verified != true) line.setSpan(
            android.text.style.ForegroundColorSpan(color(R.color.warning)),
            line.length - "Not verified".length, line.length, android.text.Spannable.SPAN_EXCLUSIVE_EXCLUSIVE,
        )
        bar.subtitle.text = line
        bar.subtitle.setTextColor(color(R.color.muted))
        clearBanner()
        if (c == null) return
        composeBar.visibility = if (c.accepted) View.VISIBLE else View.GONE
        if (!c.accepted) {
            banner(
                "${c.title} wants to message you. They can't tell whether you've read this. " +
                    "Accept to reply; block to stop them for good.",
                "Accept" to { request("accept") },
                "Block" to { request("block") },
                "Delete" to { request("delete") },
                warning = true,
            )
            return
        }
        val revealed = c.revealed
        when {
            // They reached us anonymously and then proved who they are.
            revealed != null && revealedKnown == null -> banner(
                "${c.title} proved they are ${Threnody.short(revealed)}. Add them to talk to them as themselves" +
                    (if (persona != null) " (from your main identity)." else "."),
                *(c.revealedInvite?.let { inv -> arrayOf<Pair<String, () -> Unit>>("Add them" to { addRevealed(inv) }) }
                    ?: emptyArray()),
            )
            // Someone we'd verified now has a device we haven't: a key we
            // don't know is in the conversation.
            !c.verified && c.anyVerified -> banner(
                "${c.title} added a device you haven't verified. Until you compare its safety number, " +
                    "someone else could be reading as them.",
                "Compare" to { safety() },
                warning = true,
            )
            !c.approved -> banner(
                "Approve ${c.title} once you trust them, and compare safety numbers to be sure no one is in the middle.",
                "Approve" to { setApproval(true) },
                "Compare" to { safety() },
                "Why?" to {
                    SecureBuilder(this).setTitle("Approving a contact")
                        .setMessage("Mutually approved contacts find each other nearby, relay for each other and " +
                            "hold each other's messages while offline.\n\nComparing safety numbers, in person or " +
                            "over a call, proves no one is in the middle.")
                        .setPositiveButton("OK", null).show()
                },
            )
            !c.verified -> banner(
                "Not verified. Compare safety numbers with ${c.title} to make sure no one is in the middle.",
                "Compare" to { safety() },
                warning = true,
            )
        }
        // Onion routing couldn't hide the last session or deposit: say so
        // quietly until dismissed.
        val devices = listOf(device) + c.devices
        devices.firstNotNullOfOrNull { Threnody.notices[persona to it] }?.let { notice ->
            banner(notice, "OK" to {
                devices.forEach { Threnody.notices.remove(persona to it) }
                refreshSoon()
            })
        }
    }

    private fun groupHeader(g: GroupInfo?) {
        clearBanner()
        bar.subtitle.visibility = View.VISIBLE
        callButtons.forEach { it.visibility = if (g == null) View.GONE else View.VISIBLE }
        if (g == null) {
            val invites = try { node.groupInvites().filter { it.group == group } } catch (_: Exception) { emptyList() }
            val invite = invites.firstOrNull { it.from == inviter } ?: invites.firstOrNull()
            shownInvite = invite
            if (invite != null) {
                bar.title.text = invite.name
                bar.subtitle.text = "invitation"
                banner(
                    "${Threnody.nameOf(node, invite.from)} invites you to this group. Members see each other's " +
                        "messages and who else is in it.",
                    "Join" to { joinGroup() },
                    "Decline" to { declineGroup() },
                )
            } else {
                bar.subtitle.text = "not a member"
                banner("You're no longer in this group. The owner can invite you again.")
            }
            composeBar.visibility = View.GONE
            return
        }
        composeBar.visibility = View.VISIBLE
        bar.title.text = g.name
        bar.face?.let { f ->
            if (!Themes.showPicture(f, if (ownLook) null else look?.picture)) f.show(g.name, g.id)
        }
        bar.subtitle.text = listOfNotNull(
            members(g.members.size),
            when (g.myRole) {
                MemberRole.OWNER -> "you're the owner"
                MemberRole.ADMIN -> "you're an admin"
                else -> null
            },
        ).joinToString(" · ")
        if (g.updateNeeded) {
            banner("This group's roles come from a newer Threnody. Update to change them here.", warning = true)
        }
        when (val b = g.banner) {
            is GroupBanner.OutOfStep -> banner(
                "This device fell out of step with the group. Asking the others to bring it back…", warning = true)
            is GroupBanner.CanClaim -> banner(
                "${Threnody.nameOf(node, b.owner)} hasn't been seen in this group for 14 days. You're next in line.",
                "Take over as owner" to { claimGroup() },
            )
            is GroupBanner.PendingChanges -> {
                val text = "${b.count} ${if (b.count == 1u) "change is" else "changes are"} waiting for " +
                    "${Threnody.nameOf(node, b.coordinator)} to come online."
                if (g.myRole == MemberRole.OWNER) {
                    banner(text, "Apply from this device" to {
                        Threading.background { run("apply the changes") { node.takeOverGroup(g.id) } }
                    })
                } else banner(text)
            }
            is GroupBanner.UpgradePending -> banner(
                "Admins can be added once ${b.outdated.joinToString(", ") { Threnody.nameOf(node, it) }} " +
                    "update Threnody.")
            is GroupBanner.OnlyYou -> if (g.myRole != MemberRole.MEMBER) {
                banner("Only you are in this group. Invite contacts to it.", "Invite" to { inviteToGroup() })
            } else banner("Only you are in this group.")
            null -> {}
        }
    }

    /**
     * "Carol was removed by Bob", "Carol left", or "You removed Carol" for a
     * change made on another of our devices; nothing for one made on this
     * device (the screen shows it).
     */
    private fun memberNotice(e: NodeEvent.GroupMembersChanged): String? {
        val who = (e.added + e.removed).firstOrNull() ?: return null
        val added = e.added.isNotEmpty()
        val name = Threnody.nameOf(node, who)
        val by = e.by
        // No one said, or the member itself: it joined or left.
        if (by == null || by == who) return if (added) "$name joined" else "$name left"
        val ours = try {
            if (by == node.deviceFingerprint()) return null
            node.devices().any { it.fingerprint == by }
        } catch (_: Exception) { return null }
        return when {
            ours && added -> "You added $name"
            ours -> "You removed $name"
            added -> "$name was added by ${Threnody.nameOf(node, by)}"
            else -> "$name was removed by ${Threnody.nameOf(node, by)}"
        }
    }

    /** "You are now an admin", when this device's role changes. */
    private fun roleNotice(e: NodeEvent.GroupRoleChanged): String? {
        val me = try { node.deviceFingerprint() } catch (_: Exception) { return null }
        if (e.member != me) return null
        return when (e.role.toInt()) {
            0 -> "You are now an owner"
            1 -> "You are now an admin"
            else -> "You are now a member"
        }
    }

    private fun claimGroup() {
        val g = info ?: return
        SecureBuilder(this)
            .setTitle("Take over ${g.name}?")
            .setMessage("You become its owner. The old owner becomes an admin, and this can't be undone.")
            .setPositiveButton("Take over") { _, _ ->
                Threading.background { run("take over") { node.claimGroupOwnership(g.id) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun clearBanner() {
        banner.removeAllViews()
        banner.visibility = View.GONE
    }

    /**
     * A notice above the chat, as a tinted card: an icon, what it says, and
     * its actions as buttons at its foot (the first one filled).
     */
    private fun banner(text: String, vararg actions: Pair<String, () -> Unit>, warning: Boolean = false) {
        banner.visibility = View.VISIBLE
        val tone = color(if (warning) R.color.warning else R.color.accent)
        val card = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            background = rounded(withAlpha(tone, if (isNight) 0x2E else 0x16), dp(20).toFloat())
            setPadding(dp(Design.md), dp(Design.md), dp(Design.lg), dp(Design.md))
        }
        card.addView(iconTile(if (warning) R.drawable.ic_warning else R.drawable.ic_shield, 36, tone),
            LinearLayout.LayoutParams(dp(36), dp(36)))
        val texts = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        texts.addView(label(text, 14f, R.color.text, maxLines = 0).apply {
            setLineSpacing(0f, 1.15f)
            setPadding(0, dp(Design.xs), 0, 0)
        }, matchWrap)
        if (actions.isNotEmpty()) {
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(10), 0, 0)
            }
            // The first action filled, the next tonal, the rest plain text.
            actions.forEachIndexed { i, (name, onClick) ->
                row.addView(label(name, 14f, when (i) {
                    0 -> R.color.on_accent
                    1 -> R.color.on_accent_container
                    else -> R.color.accent
                }, Design.weightBold).apply {
                    gravity = Gravity.CENTER
                    minHeight = dp(36)
                    setPadding(dp(if (i < 2) Design.lg else Design.sm), 0, dp(if (i < 2) Design.lg else Design.sm), 0)
                    background = when (i) {
                        0 -> rounded(tone.takeIf { warning } ?: color(R.color.accent), dp(18).toFloat())
                        1 -> rounded(color(R.color.accent_container), dp(18).toFloat())
                        else -> null
                    }
                    if (i == 0 && warning) setTextColor(color(R.color.md_sys_color_on_error))
                    foreground = ripple()
                    setOnClickListener { v -> Design.lightHaptic(v); onClick() }
                }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginEnd = dp(Design.sm) })
            }
            texts.addView(row, matchWrap)
        }
        card.addView(texts, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginStart = dp(Design.md) })
        banner.addView(card, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(Design.sm) })
    }

    /** A day as a chat heading: Today, Yesterday, the weekday within a week, else the date. */
    private fun dayHeading(ms: Long): String {
        if (ms <= 0) return ""
        val now = java.util.Calendar.getInstance()
        val then = java.util.Calendar.getInstance().apply { timeInMillis = ms }
        val zone = java.util.TimeZone.getDefault()
        fun dayOf(t: Long) = Math.floorDiv(t + zone.getOffset(t), DateUtils.DAY_IN_MILLIS)
        val ago = dayOf(System.currentTimeMillis()) - dayOf(ms)
        return when {
            ago == 0L -> "Today"
            ago == 1L -> "Yesterday"
            ago in 2L..6L -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_WEEKDAY)
            then.get(java.util.Calendar.YEAR) == now.get(java.util.Calendar.YEAR) ->
                DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_SHOW_WEEKDAY or DateUtils.FORMAT_ABBREV_ALL or DateUtils.FORMAT_NO_YEAR)
            else -> DateUtils.formatDateTime(this, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_SHOW_YEAR or DateUtils.FORMAT_ABBREV_MONTH)
        }
    }

    /** What the chat last drew, so a refresh that changes nothing leaves it be. */
    private var lastShown: Pair<List<Item>, Map<String, String>>? = null

    private fun show(items: List<Item>, names: Map<String, String>) {
        // Redrawing reloads photos and stills, which flickers.
        if (lastShown == items to names) return
        lastShown = items to names
        val y = scroll.scrollY
        messages.removeAllViews()
        bubbles.clear()
        mentionRanges.clear()
        linkCache.clear()
        fetchLabels.clear()
        offerLabels.clear()
        onScreen = items.map { it.entry }
        texts.clear()
        voiceViews.clear()
        if (items.isEmpty()) {
            val text = if (group != null) "No messages yet. Group messages are end-to-end encrypted with MLS."
            else "No messages yet. Messages are end-to-end encrypted and stored encrypted on this device."
            messages.addView(label(text, 14f, R.color.muted, maxLines = 0).apply { gravity = Gravity.CENTER; setPadding(dp(24), dp(48), dp(24), 0) }, matchWrap)
        }
        var lastSender: String? = null
        var lastDay = ""
        val runs = albums(items)
        // Messages from one sender within a few minutes read as one group:
        // close together, their inner corners tucked in.
        fun joins(a: List<Item>?, b: List<Item>?): Boolean {
            val x = a?.last()?.entry ?: return false
            val y = b?.first()?.entry ?: return false
            if (x.notice != null || y.notice != null) return false
            return x.outgoing == y.outgoing && x.device == y.device &&
                dayHeading(x.atMs.toLong()) == dayHeading(y.atMs.toLong()) &&
                y.atMs.toLong() - x.atMs.toLong() in 0L..GROUP_GAP_MS
        }
        for ((i, run) in runs.withIndex()) {
            val e = run.first().entry
            // A quiet heading where the day changes ("Today", "Yesterday", "Sat 21 Nov").
            val day = dayHeading(e.atMs.toLong())
            if (day != lastDay) {
                lastDay = day
                lastSender = null
                messages.addView(TextView(this).apply {
                    text = day
                    textSize = 12f
                    setTextColor(color(R.color.muted))
                    setTypeface(typeface, Typeface.BOLD)
                    background = rounded(color(R.color.card), dp(12).toFloat())
                    setPadding(dp(12), dp(5), dp(12), dp(5))
                }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply {
                    gravity = Gravity.CENTER_HORIZONTAL
                    topMargin = dp(Design.lg)
                    bottomMargin = dp(Design.sm)
                })
            }
            // A notice (a deleted message's placeholder): a quiet centred line.
            e.notice?.let { n ->
                val v = noticeView(e, n)
                messages.addView(v, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply {
                    gravity = Gravity.CENTER_HORIZONTAL
                    topMargin = dp(6)
                    bottomMargin = dp(6)
                })
                bubbles[mark(e)] = v
                lastSender = null
                continue
            }
            // In groups, name the sender above the first of their run of messages.
            val sender = if (group != null && !e.outgoing && e.device != lastSender) names[e.device] else null
            lastSender = if (e.outgoing) null else e.device
            val view = bubble(run, sender, joins(runs.getOrNull(i - 1), run), joins(run, runs.getOrNull(i + 1)))
            messages.addView(view)
            for (item in run) bubbles[mark(item.entry)] = view
        }
        messages.addView(typingBubble, matchWrap)
        if (stick && hit < 0 && jump == null || items.lastOrNull()?.sending == true) toBottom()
        // Otherwise stay where the reader is, not at the top.
        else scroll.post { scroll.scrollTo(0, y) }
    }

    /** Draws the chat's theme: its background, its accent on our bubbles, a group's picture. */
    private fun applyLook() {
        scroll.background = Themes.background(this, look, ownLook)
        // The round send and voice buttons take the accent too.
        val accent = Themes.accent(look, ownLook) ?: color(R.color.accent)
        for (b in listOf(sendButton, micButton)) {
            ((b.background as? android.graphics.drawable.RippleDrawable)?.getDrawable(0) as? android.graphics.drawable.GradientDrawable)
                ?.setColor(accent)
            b.imageTintList = android.content.res.ColorStateList.valueOf(
                if (Themes.accent(look, ownLook) != null) Themes.onColor(accent) else color(R.color.on_accent),
            )
        }
        // Bubbles take the accent: draw them again.
        lastShown = null
        if (group != null) bar.face?.let { f ->
            if (!Themes.showPicture(f, if (ownLook) null else look?.picture)) f.show(bar.title.text.toString(), key)
        }
    }

    /** The chat's look: a theme for everyone in it, or one's own plain look. */
    private fun chooseTheme() {
        val g = group
        Themes.pick(
            this, look, group = g != null, canSet = mayTheme, own = ownLook, title = bar.title.text.toString(),
            onPhoto = { picture, done ->
                val pick = Intent(Intent.ACTION_GET_CONTENT).setType("image/*").addCategory(Intent.CATEGORY_OPENABLE)
                pickFileLauncher.launch(pick) { code, data ->
                    val uri = data?.data
                    if (code != RESULT_OK || uri == null) return@launch
                    Threading.background {
                        val max = (if (picture) uniffi.threnody_ffi.maxPicturePhoto() else uniffi.threnody_ffi.maxBackgroundPhoto()).toInt()
                        val bytes = Themes.shrink(this, uri, if (picture) 512 else 1600, max)
                        runOnUiThread {
                            if (bytes == null) Toast.makeText(this, "Couldn't use that photo.", Toast.LENGTH_SHORT).show()
                            else done(bytes)
                        }
                    }
                }
            },
            setOwn = { on ->
                Threading.background {
                    run("change the look") {
                        if (g != null) node.setGroupOwnLook(g, on) else node.setOwnLook(device, on)
                    }
                    refresh()
                }
            },
        ) { theme ->
            Threading.background {
                run("set the theme") {
                    when {
                        note -> node.setNoteTheme(theme)
                        g != null -> node.setGroupTheme(g, theme)
                        else -> node.setChatTheme(device, theme)
                    }
                }
                refresh()
            }
        }
    }

    /**
     * A notice as a centred pill: its icon and what happened. A long press
     * offers to remove it from this chat (here and on our devices).
     */
    private fun noticeView(e: HistoryEntry, n: NoticeInfo): View {
        val missed = n is NoticeInfo.Call && !e.outgoing &&
            (n.outcome == CallOutcomeInfo.MISSED || n.outcome == CallOutcomeInfo.BUSY)
        val icon = when (n) {
            is NoticeInfo.Deleted -> R.drawable.ic_delete
            is NoticeInfo.Call -> if (n.video) R.drawable.ic_videocam else R.drawable.ic_call
            is NoticeInfo.Member -> R.drawable.ic_group
            is NoticeInfo.Role -> R.drawable.ic_shield
            is NoticeInfo.Timer -> R.drawable.ic_timer
            is NoticeInfo.Security -> R.drawable.ic_shield
            is NoticeInfo.Theme -> R.drawable.ic_image
        }
        return label(e.text, 13f, if (missed) R.color.warning else R.color.muted).apply {
            icon(icon, 16)
            // A call: a tap calls back, the same way.
            if (n is NoticeInfo.Call && uniffi.threnody_ffi.callsBuiltIn() && group == null && !note) {
                foreground = ripple()
                setOnClickListener { v -> Design.lightHaptic(v); call(video = n.video) }
            }
            gravity = Gravity.CENTER_VERTICAL
            background = rounded(color(R.color.card), dp(14).toFloat())
            setPadding(dp(12), dp(6), dp(14), dp(6))
            contentDescription = "${e.text}, ${time(e.atMs.toLong())}"
            setOnLongClickListener { v ->
                Design.mediumHaptic(v)
                SecureBuilder(this@ChatActivity)
                    .setItems(arrayOf("Remove")) { _, _ ->
                        Threading.background {
                            run("remove") {
                                when {
                                    note -> node.deleteNote(e.atMs, e.device)
                                    e.id != 0uL -> node.deleteMessages(device, listOf(e.id), false)
                                    else -> node.deleteEntry(device, e.atMs, e.device)
                                }
                            }
                        }
                    }
                    .show()
                true
            }
        }
    }

    /** Identifies a message across reloads and search results. */
    /** Ticks from weakest to strongest, to show an album's weakest. */
    private fun tickRank(t: TickMark) = when (t) {
        is TickMark.None -> 0
        is TickMark.Sent -> 1
        is TickMark.Partial -> 2
        is TickMark.Delivered -> 3
        is TickMark.Read -> 4
    }

    /**
     * What a file whose contents aren't on this device says about getting
     * them from another of our devices; null when none can bring them
     * (then it says, as ever, that it isn't here).
     */
    private fun fetchText(f: FileInfo): String? = fetchText(f.fetch)

    /**
     * What a file whose contents aren't on this device says: how its
     * download from the sender is going, if it was offered (too large for
     * one message), else how getting it from our other devices is.
     */
    private fun missingText(f: FileInfo, e: HistoryEntry): String? =
        offerState(e)?.let { offerText(it, f.size) } ?: fetchText(f)

    /** Where the download of `e`'s file is, if it was offered and isn't here yet. */
    private fun offerState(e: HistoryEntry): TransferState? =
        if (e.outgoing || group != null || note || e.file?.location != null) null
        else try { node.transferState(device, e.id) } catch (_: Exception) { null }

    private fun offerText(s: TransferState, size: ULong): String {
        val total = Formatter.formatShortFileSize(this, size.toLong())
        return when (s) {
            // The size is in the bubble's footer already.
            is TransferState.Idle -> "Tap to download"
            is TransferState.Waiting -> "Waiting for the sender… · tap to cancel"
            is TransferState.Fetching ->
                "Downloading… ${if (s.total == 0uL) 100uL else s.received * 100uL / s.total}% of $total · tap to cancel"
            is TransferState.NoSpace -> "Not enough space for $total · tap to try again"
            is TransferState.Unavailable -> "No longer available from the sender · tap to try again"
            is TransferState.Failed -> "Didn't arrive intact · tap to try again"
            is TransferState.Done -> "Downloaded"
        }
    }

    /** Thumbnails offered files came with, by message id (null: none). */
    private val offerThumbs = HashMap<ULong, android.graphics.Bitmap?>()

    /**
     * The thumbnail `e`'s offer came with, decoded, for `done` on the main
     * thread; nothing for a sensitive file (it stays covered until it is
     * downloaded and opened) or one that isn't an offer.
     */
    private fun offerThumbnail(f: FileInfo, e: HistoryEntry, done: (android.graphics.Bitmap) -> Unit) {
        if (f.sensitive || offerState(e) == null) return
        if (offerThumbs.containsKey(e.id)) return offerThumbs[e.id]?.let(done) ?: Unit
        val id = e.id
        Threading.io {
            val b = try {
                node.offerThumbnail(device, id)?.let { android.graphics.BitmapFactory.decodeByteArray(it, 0, it.size) }
            } catch (_: Exception) {
                null
            }
            runOnUiThread {
                // A few hundred KB each: a long scroll doesn't keep them all.
                if (offerThumbs.size >= 64) offerThumbs.clear()
                offerThumbs[id] = b
                if (b != null) done(b)
            }
        }
    }

    /** `cover` (an offered photo's label) over the thumbnail it came with, if any. */
    private fun behindOfferThumbnail(cover: TextView, f: FileInfo, e: HistoryEntry): View {
        val frame = FrameLayout(this)
        val image = ImageView(this).apply {
            scaleType = ImageView.ScaleType.CENTER_CROP
            background = rounded(color(R.color.cover), dp(12).toFloat())
            clipToOutline = true
            visibility = View.GONE
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        frame.addView(image, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        frame.addView(cover, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT))
        offerThumbnail(f, e) { b ->
            image.setImageBitmap(b)
            image.visibility = View.VISIBLE
            // The words stay readable over the picture.
            cover.background = rounded(0x99000000.toInt(), dp(12).toFloat())
            cover.setTextColor(android.graphics.Color.WHITE)
        }
        return frame
    }

    /** The thumbnail an offered file (a video, say) came with, shown above its state. */
    private fun offerImage(f: FileInfo, e: HistoryEntry): View? {
        if (f.sensitive || offerState(e) == null) return null
        val image = ImageView(this).apply {
            adjustViewBounds = true
            maxHeight = dp(220)
            // The bubble's width, cropped rather than letterboxed.
            scaleType = ImageView.ScaleType.CENTER_CROP
            background = rounded(color(R.color.surface), dp(12).toFloat())
            clipToOutline = true
            visibility = View.GONE
            contentDescription = "Preview of ${f.name}"
            setOnClickListener { offerTap(e) }
            layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(4) }
        }
        offerThumbnail(f, e) { b ->
            image.setImageBitmap(b)
            image.visibility = View.VISIBLE
        }
        return image
    }

    /** Labels of offered files shown, by message id, how each words its state, and the file's size. */
    private val offerLabels = HashMap<ULong, Triple<TextView, (String) -> String, ULong>>()

    /** A download moved on: its label says so. */
    private fun showOffer(id: ULong, s: TransferState) {
        val (view, word, size) = offerLabels[id] ?: return
        view.text = word(offerText(s, size))
    }

    /**
     * Downloads `e`'s offered file on a tap, or stops a download under
     * way (deleting what came).
     */
    private fun offerTap(e: HistoryEntry) {
        Threading.background {
            try {
                when (node.transferState(device, e.id)) {
                    is TransferState.Waiting, is TransferState.Fetching -> node.cancelDownload(device, e.id)
                    null, is TransferState.Done -> {}
                    else -> node.downloadFile(device, e.id)
                }
                val now = node.transferState(device, e.id)
                runOnUiThread { if (now != null) showOffer(e.id, now) else refreshSoon() }
            } catch (x: Exception) {
                runOnUiThread { Toast.makeText(this, x.message ?: "Couldn't download that file", Toast.LENGTH_LONG).show() }
            }
        }
    }

    private fun fetchText(s: FileFetch): String? = when (s) {
        is FileFetch.NotHere -> null
        is FileFetch.Idle -> "Tap to get from your other device"
        is FileFetch.Waiting -> "Waiting for your other device…"
        is FileFetch.Fetching -> "Downloading… ${if (s.total == 0uL) 100uL else s.received * 100uL / s.total}%"
        is FileFetch.Unavailable -> "Not on your other devices"
    }

    /** Labels of files coming from our other devices, by entry, and how each words its state. */
    private val fetchLabels = HashMap<String, Pair<TextView, (String) -> String>>()
    private val fetchPoll = Handler(Looper.getMainLooper())

    /**
     * Makes `view` (a missing file's label) get the file from another of
     * our devices on a tap, while it can, and keep its progress current.
     */
    private fun fetchable(view: TextView, f: FileInfo, e: HistoryEntry, word: (String) -> String) {
        if (offerState(e) != null) {
            offerLabels[e.id] = Triple(view, word, f.size)
            view.setOnClickListener { offerTap(e) }
            return
        }
        if (f.fetch is FileFetch.Idle || f.fetch is FileFetch.Unavailable) view.setOnClickListener { fetch(e) }
        if (f.fetch is FileFetch.Waiting || f.fetch is FileFetch.Fetching) {
            fetchLabels[mark(e)] = view to word
            pollFetches()
        }
    }

    /** Asks our other devices for `e`'s file. */
    private fun fetch(e: HistoryEntry) {
        val g = group
        Threading.background {
            try {
                when {
                    note -> node.fetchNoteFile(e.atMs, e.device)
                    g != null -> node.fetchGroupFile(g, e.atMs, e.device)
                    else -> node.fetchFile(device, e.atMs, e.device)
                }
            } catch (x: Exception) {
                runOnUiThread { Toast.makeText(this, x.message ?: "Couldn't get that file", Toast.LENGTH_LONG).show() }
            }
            refresh()
        }
        pollFetches()
    }

    /**
     * Updates download progress once a second while anything shown is
     * coming or waiting (cheap: no history is read).
     */
    private fun pollFetches() {
        if (fetchPoll.hasMessages(FETCH_POLL)) return
        fetchPoll.sendMessageDelayed(android.os.Message.obtain(fetchPoll) {
            Threading.background {
                val now = try { node.fileFetches() } catch (_: Exception) { emptyList() }
                runOnUiThread {
                    val byEntry = now.associateBy { mark(it.atMs, it.device) }
                    var coming = false
                    var finished = false
                    for ((k, v) in fetchLabels) {
                        val p = byEntry[k]
                        // Gone from the queue, or nobody has it: redrawn
                        // as it now is (with its tap to try again).
                        if (p == null || p.fetch is FileFetch.Unavailable) { finished = true; continue }
                        fetchText(p.fetch)?.let { v.first.text = v.second(it) }
                        if (p.fetch is FileFetch.Fetching || p.fetch is FileFetch.Waiting) coming = true
                    }
                    if (finished) refreshSoon()
                    if (coming) pollFetches()
                }
            }
        }.apply { what = FETCH_POLL }, 1000)
    }

    private fun mark(e: HistoryEntry) = mark(e.atMs, e.device)
    private fun mark(atMs: ULong, device: String) = "$atMs/$device"

    /** Searches this chat (on the worker thread); shows the newest match. */
    private fun find(q: String) {
        val t = Search.terms(q)
        val n = ++searches
        if (t.isEmpty()) return found(n, t, emptyList())
        val g = group
        val d = device
        Threading.background {
            if (!::node.isInitialized) return@background
            val list = try {
                if (g != null) node.searchGroupMessages(g, q, SEARCH_LIMIT) else node.searchMessages(d, q, SEARCH_LIMIT)
            } catch (e: Exception) {
                Threnody.say("! search: ${e.message}")
                emptyList()
            }
            runOnUiThread { found(n, t, list) }
        }
    }

    private fun found(n: Int, t: List<String>, list: List<HistoryEntry>) {
        if (n != searches || !search.isOpen) return
        terms = t
        hits = list
        // Opened from a search result: start at that one.
        val wanted = openedAt ?: jump?.first
        openedAt = null
        hit = list.indexOfFirst { mark(it) == wanted }.takeIf { it >= 0 } ?: if (list.isEmpty()) -1 else 0
        search.setCount(hit, list.size)
        paint()
        if (hit >= 0) goTo(list[hit])
    }

    /** To an older (+1) or newer (-1) match. */
    private fun step(by: Int) {
        val i = hit + by
        if (i !in hits.indices) return
        hit = i
        search.setCount(hit, hits.size)
        paint()
        goTo(hits[i])
    }

    private fun endSearch() {
        searches++
        hits = emptyList()
        hit = -1
        terms = emptyList()
        jump = null
        paint()
    }

    /** Scrolls to a match, loading older history first if it isn't loaded. */
    private fun goTo(e: HistoryEntry) {
        val k = mark(e)
        if (k in bubbles) {
            jump = null
            return reveal(k)
        }
        jump = k to e.atMs
        if (!allLoaded && oldestLoaded > e.atMs) loadUntil(e.atMs)
    }

    /** Loads more history, a page at a time, until it reaches `at` (then refreshes). */
    private fun loadUntil(at: ULong) {
        val g = group
        val d = device
        Threading.background {
            var limit = historyLimit
            while (limit < MAX_HISTORY) {
                limit = minOf(limit * 2u, MAX_HISTORY)
                val h = try { if (g != null) node.groupHistory(g, limit) else node.history(d, limit) } catch (_: Exception) { break }
                if (h.size.toUInt() < limit || (h.firstOrNull()?.atMs ?: 0uL) <= at) break
            }
            historyLimit = limit
            refresh()
        }
    }

    /** Marks the search terms in every bubble's text; the current match stands out. */
    private fun paint() {
        for ((k, views) in texts) for ((v, raw) in views) v.text = marked(k, raw)
    }

    private fun marked(k: String, raw: String): CharSequence {
        val base = if (terms.isEmpty()) raw else Search.highlight(raw, terms, k == hits.getOrNull(hit)?.let(::mark))
        val m = mentionRanges[k]
        val styled = if (m != null && m.first == raw) Mentions.style(base, m.second, color(R.color.accent)) else base
        return Links.clickable(this, styled, links(raw))
    }

    /** The links in a bubble's text, found once (search repaints often). */
    private fun links(raw: String) = linkCache.getOrPut(raw) { Links.of(raw) }

    /** A bubble's text, kept so search can mark matches in it; its links take taps. */
    private fun TextView.searchable(e: HistoryEntry, raw: String): TextView {
        val k = mark(e)
        texts.getOrPut(k) { mutableListOf() }.add(this to raw)
        text = marked(k, raw)
        if (links(raw).isNotEmpty()) Links.enable(this)
        return this
    }

    /** Scrolls a message into view (a third of the way down) and flashes it. */
    private fun reveal(k: String) {
        val row = bubbles[k] ?: return
        val go = {
            scroll.smoothScrollTo(0, maxOf(0, row.top - scroll.height / 3))
            flash(row)
        }
        if (row.isLaidOut && !row.isLayoutRequested) row.post(go)
        else row.addOnLayoutChangeListener(object : View.OnLayoutChangeListener {
            override fun onLayoutChange(v: View, l: Int, t: Int, r: Int, b: Int, ol: Int, ot: Int, oldR: Int, ob: Int) {
                v.removeOnLayoutChangeListener(this)
                v.post(go)
            }
        })
    }

    /** A brief tint behind a message, to show which one a search landed on. */
    private fun flash(v: View) {
        val tint = android.graphics.drawable.ColorDrawable(color(R.color.accent)).apply { alpha = 0 }
        v.background = tint
        android.animation.ValueAnimator.ofInt(0, 70, 0).apply {
            duration = 1_200L
            startDelay = Design.durationNormal.toLong()
            addUpdateListener { tint.alpha = it.animatedValue as Int }
            start()
        }
    }

    // Not fullScroll(): that moves focus to the last bubble, away from the compose field.
    private fun toBottom() {
        stick = true
        scroll.post { scroll.scrollTo(0, messages.height) }
    }

    /** Consecutive files of one album from one sender go in one bubble. */
    private fun albums(items: List<Item>): List<List<Item>> {
        val out = mutableListOf<MutableList<Item>>()
        for (item in items) {
            val album = item.entry.file?.album ?: 0uL
            val last = out.lastOrNull()?.last()?.entry
            if (album != 0uL && last?.file?.album == album && last.device == item.entry.device) {
                out.last().add(item)
            } else {
                out.add(mutableListOf(item))
            }
        }
        return out
    }

    /** Photos of one message: one large, or a grid of several. */
    private fun photos(run: List<Item>): View {
        val files = run.mapNotNull { it.entry.file?.let { f -> f to it.entry } }
        val width = minOf(dp(260), resources.displayMetrics.widthPixels - dp(120))
        val single = files.size == 1
        val side = if (single) width else (width - dp(4)) / 2
        val grid = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        for (row in files.chunked(if (single) 1 else 2)) {
            val line = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
            for ((i, pair) in row.withIndex()) {
                val (f, e) = pair
                val cell = photo(f, e, side, if (single) dp(320) else side)
                line.addView(cell, LinearLayout.LayoutParams(side, if (single) WRAP_CONTENT else side).apply {
                    if (i == 1) marginStart = dp(4)
                })
            }
            grid.addView(line, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { bottomMargin = dp(4) })
        }
        return grid
    }

    /**
     * One photo. A sensitive one is an opaque cover: it isn't even decoded
     * until opened, so nothing of it shows in the chat.
     */
    private fun photo(f: uniffi.threnody_ffi.FileInfo, e: HistoryEntry, width: Int, limit: Int): View {
        val location = f.location
        val open = View.OnClickListener {
            if (location == null) return@OnClickListener
            // For a result: a photo edited there comes back here to send.
            viewPhotoLauncher.launch(
                Intent(this, ImageActivity::class.java)
                    .putExtra(ImageActivity.LOCATION, location)
                    .putExtra(ImageActivity.NAME, f.name)
                    .putExtra(ImageActivity.CAPTION, e.text)
            ) { resultCode, data ->
                val path = data?.getStringExtra(AnnotateActivity.RESULT_PATH)
                // An edited copy of a sensitive photo starts out sensitive too.
                if (resultCode == RESULT_OK && path != null) sendEdited(path, data.getStringExtra(ImageActivity.NAME) ?: "photo.jpg", f.sensitive)
            }
        }
        if (f.sensitive || location == null) {
            val cover = label(if (location == null) missingText(f, e) ?: "not available" else "Sensitive photo\nTap to view", 14f, R.color.on_cover).apply {
                icon(if (location == null) R.drawable.ic_image else R.drawable.ic_lock, 28, top = true)
                gravity = Gravity.CENTER
                setTypeface(typeface, Typeface.BOLD)
                // Opaque and dark in either theme: covered, without glare.
                background = rounded(color(R.color.cover), dp(12).toFloat()).apply {
                    setStroke(dp(1), color(R.color.cover_edge))
                }
                height = if (limit == width) width else dp(180)
                contentDescription = if (location == null) "Photo: ${missingText(f, e) ?: "not available"}" else "Sensitive photo, tap to view"
                setOnClickListener(open)
                if (location == null) fetchable(this, f, e) { it }
            }
            return if (location == null) behindOfferThumbnail(cover, f, e) else cover
        }
        val view = ImageView(this).apply {
            adjustViewBounds = true
            maxHeight = limit
            scaleType = if (limit == width) ImageView.ScaleType.CENTER_CROP else ImageView.ScaleType.FIT_CENTER
            background = rounded(color(R.color.surface), dp(12).toFloat())
            clipToOutline = true
            minimumHeight = dp(120)
            contentDescription = e.text.ifBlank { "Photo" }
            setOnClickListener(open)
        }
        val still = { Media.thumbnail(this, location, width) { b -> runOnUiThread { view.setImageBitmap(b); view.minimumHeight = 0 } } }
        if (!Media.isGif(f.name)) still()
        else Media.animated(this, location, width) { d ->
            if (d == null) return@animated still()
            runOnUiThread {
                view.setImageDrawable(d)
                view.minimumHeight = 0
                (d as android.graphics.drawable.AnimatedImageDrawable).start()
            }
        }
        if (Media.isGif(f.name)) view.contentDescription = e.text.ifBlank { "GIF" }
        return view
    }

    /**
     * One message (or album) as a bubble. `joinsPrev` and `joinsNext` say it
     * runs on from, or into, the same sender's neighbours: its corners on
     * the sender's side are tucked in there, and the gap is small.
     */
    private fun bubble(run: List<Item>, sender: String?, joinsPrev: Boolean = false, joinsNext: Boolean = false): View {
        val item = run.first()
        val e = item.entry
        val outgoing = e.outgoing
        val accent = Themes.accent(look, ownLook)
        val fg = when {
            outgoing && accent != null -> Themes.onColor(accent)
            outgoing -> color(R.color.on_bubble_out)
            else -> color(R.color.text)
        }
        val big = dp(20).toFloat()
        val small = dp(6).toFloat()
        val top = if (joinsPrev) small else big
        val bottom = if (joinsNext) small else big
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = android.graphics.drawable.GradientDrawable().apply {
                setColor(if (outgoing) accent ?: color(R.color.bubble_out) else color(R.color.bubble_in))
                // Corners: top-left, top-right, bottom-right, bottom-left (x, y each).
                cornerRadii = if (outgoing) floatArrayOf(big, big, top, top, bottom, bottom, big, big)
                    else floatArrayOf(top, top, big, big, big, big, bottom, bottom)
            }
            setPadding(dp(14), dp(8), dp(14), dp(6))
        }
        if (sender != null) {
            body.addView(label(sender, 13f, R.color.accent).apply { setTypeface(typeface, Typeface.BOLD) })
        }
        if (e.forwarded) {
            body.addView(TextView(this).apply {
                text = "Forwarded"
                setCompoundDrawablesRelativeWithIntrinsicBounds(getDrawable(R.drawable.ic_forward)?.mutate()?.apply {
                    setTint(fg)
                    setBounds(0, 0, dp(14), dp(14))
                }, null, null, null)
                compoundDrawablePadding = dp(4)
                textSize = 12f
                setTextColor(fg)
                alpha = 0.7f
                setTypeface(typeface, Typeface.ITALIC)
            })
        }
        e.quote?.let { body.addView(quoteView(it, fg), matchWrap) }
        val file = e.file
        val time = if (item.sending) "sending…" else time(e.atMs.toLong())
        val pictures = run.all { it.entry.file?.let { f -> Media.isImage(f.name) } == true }
        val clip = file?.clip
        val meta = if (file != null && clip != null) {
            body.addView(clipView(file, clip, e, fg))
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    searchable(e, e.text)
                    textSize = 16f
                    setTextColor(fg)
                })
            }
            time
        } else if (file != null && pictures) {
            body.setPadding(dp(4), dp(4), dp(4), dp(6))
            body.addView(photos(run))
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    searchable(e, e.text)
                    textSize = 16f
                    setTextColor(fg)
                    setPadding(dp(10), dp(4), dp(10), 0)
                })
            }
            time
        } else if (file == null && e.card != null) {
            body.addView(Cards.view(this, e.card!!, outgoing, fg, connect = { addFromCard(it) }) { openCard(e) }, matchWrap)
            time
        } else if (file == null && e.poll != null) {
            val p = e.poll!!
            body.addView(TextView(this).apply {
                searchable(e, e.text)
                textSize = 16f
                setTextColor(fg)
                setTypeface(typeface, Typeface.BOLD)
            })
            body.addView(Polls.view(this, p, fg, color(R.color.accent)) { choices ->
                val g = group ?: return@view
                Threading.background { run("vote") { node.voteInGroup(g, e.id, choices) } }
            }, matchWrap)
            time
        } else if (file == null && e.list != null) {
            val listId = e.list!!
            body.addView(TextView(this).apply {
                searchable(e, e.text)
                textSize = 16f
                setTextColor(fg)
                setTypeface(typeface, Typeface.BOLD)
            })
            body.addView(TextView(this).apply {
                text = if (outgoing) "You shared a list · tap to open" else "Shared a list · tap to open"
                textSize = 13f
                setTextColor(fg)
                alpha = 0.75f
            })
            // The bubble's texts take taps (see the long press below).
            val open = View.OnClickListener { Lists.open(this, listId) }
            body.setOnClickListener(open)
            for (i in 0 until body.childCount) body.getChildAt(i).setOnClickListener(open)
            time
        } else if (file == null) {
            val (shown, ranges) = Mentions.display(e) { Threnody.nameOf(node, it) }
            if (ranges.isNotEmpty()) mentionRanges[mark(e)] = shown to ranges
            body.addView(TextView(this).apply {
                searchable(e, shown)
                textSize = 16f
                setTextColor(fg)
            })
            e.preview?.let { body.addView(Links.card(this, it, fg)) }
            time
        } else {
            for (r in run) {
                val f = r.entry.file ?: continue
                val uri = f.location?.let(Uri::parse)
                // A file as a card: a round tile (a page once it's here, an
                // arrow while it's to fetch), its name, and what to do.
                val names = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
                names.addView(TextView(this).apply {
                    searchable(r.entry, (if (f.sensitive) "Sensitive file: " else "") + f.name)
                    textSize = 15f
                    setTextColor(fg)
                    setTypeface(typeface, Typeface.BOLD)
                    maxLines = 2
                    ellipsize = android.text.TextUtils.TruncateAt.MIDDLE
                    if (uri != null) setOnClickListener { open(uri) }
                })
                if (f.location == null) missingText(f, r.entry)?.let { t ->
                    names.addView(TextView(this).apply {
                        text = t
                        textSize = 13f
                        setTextColor(fg)
                        alpha = 0.8f
                        fetchable(this, f, r.entry) { it }
                    })
                }
                body.addView(LinearLayout(this).apply {
                    orientation = LinearLayout.HORIZONTAL
                    gravity = Gravity.CENTER_VERTICAL
                    setPadding(0, dp(2), 0, dp(4))
                    addView(iconTile(if (f.location == null) R.drawable.ic_download else R.drawable.ic_file, 44, fg, withAlpha(fg, 0x2A)))
                    addView(names, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginStart = dp(Design.md) })
                    if (uri != null) setOnClickListener { open(uri) }
                    // The tile and name fetch it too, as its status line does.
                    else names.getChildAt(1)?.let { status -> setOnClickListener { status.performClick() } }
                })
                if (f.location == null) offerImage(f, r.entry)?.let { body.addView(it) }
            }
            if (e.text.isNotBlank()) {
                body.addView(TextView(this).apply {
                    searchable(e, e.text)
                    textSize = 16f
                    setTextColor(fg)
                })
            }
            Formatter.formatShortFileSize(this, run.sumOf { it.entry.file?.size ?: 0uL }.toLong()) + " · " + time +
                if (file.location?.startsWith("content:") == true && !outgoing) " · in Downloads" else ""
        }
        // One tick once sent, two once delivered: to a device of the
        // contact, or to every member of a group (with a count until then).
        // An eye once the contact has read it. An album shows its weakest.
        // Sent from another of our devices: its ticks as that device passed
        // them on, never a lone "sent", which only that device knows.
        val elsewhere = outgoing && !item.sending && e.device != myDevice
        val marks = when (val t = run.map { it.entry.tick }.minByOrNull(::tickRank) ?: e.tick) {
            is TickMark.Read -> " ✓✓ Seen"
            is TickMark.Delivered -> " ✓✓"
            is TickMark.Partial -> " ✓ ${t.delivered}/${t.recipients}"
            is TickMark.Sent -> " ✓"
            else -> ""
        }
        val tick = when {
            !outgoing || item.sending -> ""
            elsewhere -> " · from your other device$marks"
            else -> marks
        }
        reactionChips(e, fg)?.let { body.addView(it, matchWrap) }
        body.addView(TextView(this).apply {
            val metaText = meta + (if (e.edited) " · edited" else "") + (if (e.disappearing) " · ⏱" else "") + tick
            contentDescription = metaText.replace("✓✓ Seen", "read").replace("✓✓", "delivered").replace(" ✓ ", " delivered to ").replace("✓", "sent")
            textSize = 11f
            setTextColor(fg)
            alpha = 0.7f
            gravity = Gravity.END
            text = metaText
        }, matchWrap)
        if (!item.sending) {
            val longPress = View.OnLongClickListener { Links.longPressed(); deleteMessage(run.map { it.entry }); true }
            body.setOnLongClickListener(longPress)
            // Photos take the long press too, not just the bubble's edge.
            fun all(v: View) {
                if (v is ImageView || v is TextView) v.setOnLongClickListener(longPress)
                if (v is android.view.ViewGroup) for (i in 0 until v.childCount) all(v.getChildAt(i))
            }
            all(body)
        }
        val row = LinearLayout(this).apply {
            gravity = if (outgoing) Gravity.END else Gravity.START
            setPadding(if (outgoing) dp(56) else 0, dp(if (joinsPrev) 1 else 4), if (outgoing) 0 else dp(56), dp(if (joinsNext) 1 else 4))
            addView(body, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        // Swiping a message to the right replies to it.
        return if (item.sending) row else SwipeToReply(this, row, color(R.color.muted)) { replyingTo(e) }
    }

    /** Who sent a quoted message, by device: us, or their name. */
    private fun quoteAuthor(author: String) =
        if (author == myDevice) "You" else Threnody.nameOf(node, author)

    /** What a reply quotes, above its text; tapping it shows the original. */
    private fun quoteView(q: QuoteInfo, fg: Int): View = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        background = rounded(androidx.core.graphics.ColorUtils.setAlphaComponent(fg, 0x22), dp(8).toFloat())
        setPadding(dp(10), dp(6), dp(10), dp(6))
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(4) }
        addView(TextView(this@ChatActivity).apply {
            text = quoteAuthor(q.author)
            textSize = 13f
            setTextColor(if (fg == color(R.color.text)) color(R.color.accent) else fg)
            setTypeface(typeface, Typeface.BOLD)
        })
        addView(TextView(this@ChatActivity).apply {
            text = q.text.ifEmpty { "Original message not found" }
            textSize = 14f
            setTextColor(fg)
            alpha = if (q.text.isEmpty()) 0.6f else 0.85f
            if (q.text.isEmpty()) setTypeface(typeface, Typeface.ITALIC)
            maxLines = 3
            ellipsize = android.text.TextUtils.TruncateAt.END
        })
        contentDescription = "Reply to ${quoteAuthor(q.author)}: ${q.text}"
        val show = View.OnClickListener {
            val original = onScreen.lastOrNull { quotes(q, it) }
            if (original != null) reveal(mark(original))
            else Toast.makeText(this@ChatActivity, "The original isn't loaded here.", Toast.LENGTH_SHORT).show()
        }
        // The bubble's long press makes its text take touches: the quote's
        // text has to answer taps too.
        setOnClickListener(show)
        for (i in 0 until childCount) getChildAt(i).setOnClickListener(show)
    }

    /** Shows the pinned messages (newest first in the bar). */
    private fun showPins(list: List<HistoryEntry>) {
        if (list.map { it.id } != pins.map { it.id }) pinShown = 0
        pins = list
        pinBar.removeAllViews()
        if (list.isEmpty()) {
            pinBar.visibility = View.GONE
            return
        }
        val e = list[list.size - 1 - pinShown.coerceIn(0, list.size - 1)]
        pinBar.addView(ImageView(this).apply {
            setImageResource(R.drawable.ic_pin)
            imageTintList = android.content.res.ColorStateList.valueOf(color(R.color.accent))
        }, LinearLayout.LayoutParams(dp(20), dp(20)).apply { marginEnd = dp(12) })
        pinBar.addView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(label(if (list.size > 1) "Pinned message ${pinShown + 1} of ${list.size}" else "Pinned message",
                12f, R.color.accent).apply { setTypeface(typeface, Typeface.BOLD) })
            addView(label(snippet(e), 14f, R.color.text).apply {
                maxLines = 1
                ellipsize = android.text.TextUtils.TruncateAt.END
            })
        }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        pinBar.contentDescription = "Pinned: ${snippet(e)}. Tap to show it."
        pinBar.visibility = View.VISIBLE
    }

    /** Shows the pinned message in the bar, then puts the next one there. */
    private fun nextPin() {
        if (pins.isEmpty()) return
        val e = pins[pins.size - 1 - pinShown.coerceIn(0, pins.size - 1)]
        val k = mark(e)
        if (k in bubbles) reveal(k) else {
            jump = k to e.atMs
            if (!allLoaded && oldestLoaded > e.atMs) loadUntil(e.atMs)
        }
        pinShown = (pinShown + 1) % pins.size
        showPins(pins)
    }

    private fun pin(e: HistoryEntry, pin: Boolean) {
        val g = group
        Threading.background {
            run(if (pin) "pin" else "unpin") {
                when {
                    note -> node.pinNote(e.id, pin)
                    g != null -> node.pinInGroup(g, e.id, pin)
                    else -> node.pin(device, e.id, pin)
                }
            }
        }
    }

    /** What a quote of `e` shows: its text, or a file's name. */
    private fun snippet(e: HistoryEntry) = e.text.ifEmpty { e.file?.name ?: "" }

    /** Whether `q` quotes `e`: by id, or for a message too old to have one, by sender and text. */
    private fun quotes(q: QuoteInfo, e: HistoryEntry) =
        if (q.id != 0uL) e.id == q.id
        else e.device == q.author && q.text.isNotEmpty() && snippet(e).startsWith(q.text)

    /** Starts (or with null, cancels) a reply to `e`. */
    private fun replyingTo(e: HistoryEntry?, part: String? = null) {
        replyTo = e
        replyPart = part
        replyBar.removeAllViews()
        if (e == null) {
            replyBar.visibility = View.GONE
            return
        }
        val snippet = part?.let { "“$it”" } ?: snippet(e)
        replyBar.addView(View(this).apply { setBackgroundColor(color(R.color.accent)) },
            LinearLayout.LayoutParams(dp(3), MATCH_PARENT).apply { marginEnd = dp(10) })
        replyBar.addView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(label("Replying to ${if (e.outgoing) "yourself" else quoteAuthor(e.device)}", 13f, R.color.accent)
                .apply { setTypeface(typeface, Typeface.BOLD) })
            addView(label(snippet, 14f, R.color.muted).apply {
                maxLines = 2
                ellipsize = android.text.TextUtils.TruncateAt.END
            })
        }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        replyBar.addView(ImageButton(this).apply {
            setImageResource(R.drawable.ic_close)
            imageTintList = android.content.res.ColorStateList.valueOf(color(R.color.muted))
            contentDescription = "Cancel the reply"
            background = ripple(borderless = true)
            setOnClickListener { replyingTo(null) }
        }, LinearLayout.LayoutParams(dp(Design.touchTargetMin), dp(Design.touchTargetMin)))
        replyBar.visibility = View.VISIBLE
        compose.requestFocus()
        getSystemService(android.view.inputmethod.InputMethodManager::class.java)
            ?.showSoftInput(compose, android.view.inputmethod.InputMethodManager.SHOW_IMPLICIT)
    }

    /** Delete for me, or (our own 1:1 messages) for everyone; an album goes whole. */
    private fun deleteMessage(entries: List<HistoryEntry>) {
        val e = entries.first()
        val g = group
        val mine = g == null && !note && e.outgoing && e.device == myDevice && entries.all { it.id != 0uL }
        val canEdit = mine && e.file == null && e.card == null
        val options = buildList {
            add("Reply")
            // Text isn't selectable in the bubble (it would take the long
            // press), so selecting (to copy or quote a part) is offered here.
            if (e.text.isNotEmpty()) add("Select text")
            if (e.poll == null && e.card?.noForward != true) add("Forward")
            if (e.id != 0uL && mayPin) add(if (pins.any { it.id == e.id }) "Unpin" else "Pin")
            if (e.poll != null && e.outgoing && e.poll?.closed == false) add("End poll")
            if (e.text.isNotEmpty()) add("Copy text")
            // A file kept privately (sealed, so out of other apps' reach):
            // a copy can go to Downloads. One already there needs none.
            if (entries.size == 1 && e.file?.location?.startsWith("file:") == true) add("Save to Downloads")
            if (canEdit) add("Edit")
            add("Delete for me")
            if (mine) {
                add("Delete for everyone")
                add("Delete without a trace")
            }
        }
        fun pick(option: String) {
            if (option == "Edit") return editMessage(e)
            if (option == "Reply") return replyingTo(e)
            if (option == "Forward") return forward(entries)
            if (option == "Pin" || option == "Unpin") return pin(e, option == "Pin")
            if (option == "End poll") {
                val g = group ?: return
                Threading.background { run("end the poll") { node.closeGroupPoll(g, e.id) } }
                return
            }
            if (option == "Copy text") {
                val clip = android.content.ClipData.newPlainText("message", e.text)
                // Disappearing messages (and every copy with screen security
                // on) are marked sensitive: Android 13+ then doesn't show
                // the text in its clipboard preview, and keyboards with a
                // clipboard history are asked not to keep it.
                if (android.os.Build.VERSION.SDK_INT >= 33 && (e.disappearing || Privacy.screenSecurity(this))) {
                    clip.description.extras = android.os.PersistableBundle().apply {
                        putBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE, true)
                    }
                }
                getSystemService(android.content.ClipboardManager::class.java)?.setPrimaryClip(clip)
                return
            }
            if (option == "Select text") return selectText(e)
            if (option == "Save to Downloads") return saveToDownloads(e)
            val noTrace = option == "Delete without a trace"
            val everyone = option == "Delete for everyone" || noTrace
            Threading.background {
                run("delete") {
                    when {
                        note -> entries.forEach { node.deleteNote(it.atMs, it.device) }
                        g != null -> entries.forEach { node.deleteGroupEntry(g, it.atMs, it.device) }
                        entries.all { it.id != 0uL } -> node.deleteMessages(device, entries.map { it.id }, everyone, noTrace)
                        else -> entries.forEach { node.deleteEntry(device, it.atMs, it.device) }
                    }
                }
                entries.forEach { Media.forget(this, it.file?.location) }
                if (everyone) runOnUiThread {
                    Toast.makeText(this, if (noTrace) {
                        "Deleted without a trace where it hadn't been seen yet. Anyone who saw it sees “This message was deleted”."
                    } else "Deleted. They see “This message was deleted” in its place.", Toast.LENGTH_LONG).show()
                }
            }
        }
        // Reactions toggle in place (the panel stays open); actions close it.
        ReactionPanel(
            this,
            mine = e.reactions.filter { it.mine }.map { it.emoji }.toSet(),
            canReact = e.id != 0uL && !note,
            actions = options.map { option -> option to { pick(option) } },
        ) { emoji, add -> react(e, emoji, add) }.show()
    }

    /**
     * Copies a message's privately kept file to Downloads/Threnody. A
     * sensitive or disappearing one is asked about first: the copy outlives
     * the message, and any app can read it.
     */
    private fun saveToDownloads(e: HistoryEntry) {
        val f = e.file ?: return
        val location = f.location ?: return
        val save = {
            if (f.size > SAVE_NOTICE) Toast.makeText(this, "Saving ${f.name}…", Toast.LENGTH_SHORT).show()
            Threading.background {
                val ok = Media.saveToDownloads(this, location, f.name)
                runOnUiThread {
                    Toast.makeText(this, if (ok) "Saved to Downloads/Threnody" else "Couldn't save", Toast.LENGTH_SHORT).show()
                }
            }
        }
        if (!f.sensitive && !e.disappearing) return save()
        SecureBuilder(this)
            .setTitle("Save to Downloads?")
            .setMessage(
                (if (f.sensitive) "This file was sent as sensitive. " else "This message disappears. ") +
                    "The copy in Downloads stays after the message is gone, and any app with access to your files can read it."
            )
            .setPositiveButton("Save") { _, _ -> save() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /**
     * Shows a message's text where any part of it can be selected, to copy
     * or to reply quoting just that part (*Quote* by the selection's Copy).
     */
    private fun selectText(e: HistoryEntry) {
        var dialog: android.app.Dialog? = null
        val view = TextView(this).apply {
            text = e.text
            textSize = 16f
            setTextColor(color(R.color.text))
            setTextIsSelectable(true)
            setPadding(dp(24), dp(16), dp(24), dp(8))
        }
        // A message without an id is found by the start of its text, so
        // it's only ever quoted whole.
        if (e.id != 0uL) view.customSelectionActionModeCallback = object : android.view.ActionMode.Callback {
            override fun onCreateActionMode(mode: android.view.ActionMode, menu: android.view.Menu): Boolean {
                menu.add(android.view.Menu.NONE, QUOTE_ITEM, 0, "Quote")
                return true
            }
            override fun onPrepareActionMode(mode: android.view.ActionMode, menu: android.view.Menu) = false
            override fun onActionItemClicked(mode: android.view.ActionMode, item: android.view.MenuItem): Boolean {
                if (item.itemId != QUOTE_ITEM) return false
                val (a, b) = view.selectionStart to view.selectionEnd
                val part = if (a in 0..<b) view.text.substring(a, b).trim().ifEmpty { null } else null
                mode.finish()
                dialog?.dismiss()
                replyingTo(e, part)
                compose.requestFocus()
                return true
            }
            override fun onDestroyActionMode(mode: android.view.ActionMode) {}
        }
        dialog = SecureBuilder(this)
            .setView(android.widget.ScrollView(this).apply { addView(view) })
            .setPositiveButton("Done", null)
            .show()
    }

    /** Adds or takes away our reaction (several per message are fine). */
    private fun react(e: HistoryEntry, emoji: String, add: Boolean) {
        val g = group
        Threading.background {
            run("react") {
                val ok = if (g != null) node.reactInGroup(g, e.id, emoji, add) else node.react(device, e.id, emoji, add)
                if (!ok) throw IllegalStateException("that message is gone")
            }
        }
    }

    /** Reactions under a message: one chip per emoji; ours stand out and tap to take back. */
    private fun reactionChips(e: HistoryEntry, fg: Int): View? {
        if (e.reactions.isEmpty()) return null
        val row = android.widget.HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        val chips = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; setPadding(0, dp(4), 0, 0) }
        for (r in e.reactions) {
            chips.addView(TextView(this).apply {
                text = if (r.count > 1u) "${r.emoji} ${r.count}" else r.emoji
                textSize = 14f
                setTextColor(fg)
                setPadding(dp(8), dp(2), dp(8), dp(2))
                background = rounded(color(if (r.mine) R.color.accent else R.color.divider), dp(12).toFloat()).apply {
                    alpha = if (r.mine) 90 else 255
                }
                contentDescription = "${r.emoji}, ${r.count}" + if (r.mine) ", yours: tap to take back" else ", tap to add yours"
                setOnClickListener { react(e, r.emoji, !r.mine) }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginEnd = dp(4) })
        }
        row.addView(chips)
        return row
    }

    private fun editMessage(e: HistoryEntry) {
        val field = EditText(this).apply {
            setText(e.text)
            setSelection(e.text.length)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 10)
        }
        SecureBuilder(this)
            .setTitle("Edit message")
            .setMessage("They see it marked as edited, if they're running a current version.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val body = field.text.toString().trim()
                if (body.isEmpty() || body == e.text) return@setPositiveButton
                Threading.background { run("edit") { node.editMessage(device, e.id, body) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun time(ms: Long) = DateFormat.getTimeFormat(this).format(Date(ms))

    private fun send() {
        val raw = compose.text.toString()
        val text = raw.trim()
        if (text.isEmpty() || !::node.isInitialized) return
        val mentions = if (group != null) Mentions.of(compose.text, raw.indexOf(text), text) else emptyList()
        val to = replyTo
        val part = replyPart
        val quote = to?.let { QuoteInfo(it.id, it.device, (part ?: snippet(it)).take(MAX_QUOTE_CHARS)) }
        val item = text to quote
        val card = previewBar.take(text)
        replyingTo(null)
        typingHandler.removeCallbacks(stopTyping)
        compose.setText("") // also says we stopped typing
        Threnody.touch()
        synchronized(pending) { pending.add(item) }
        refreshSoon()
        Threading.background {
            val g = group
            val error = try {
                if (note) {
                    if (card != null) node.writeNoteWithPreview(text, to?.atMs, to?.device, card, part)
                    else node.writeNote(text, to?.atMs, to?.device, part)
                    null
                } else if (g != null) {
                    if (card != null) node.sendGroupTextWithPreview(g, text, to?.atMs, to?.device, mentions, card, part)
                    else if (to != null || mentions.isNotEmpty()) node.sendGroupMessage(g, text, to?.atMs, to?.device, mentions, part)
                    else node.sendGroupText(g, text)
                    null
                } else if ((when {
                        card != null -> node.sendTextWithPreview(device, text, to?.atMs, to?.device, card, part)
                        to != null -> node.sendReply(device, text, to.atMs, to.device, part)
                        else -> node.sendText(device, text)
                    }) == 0u) {
                    // Reaches the contact directly, by relay, or sealed for mailboxes.
                    "Not delivered: they're offline and no mutual contact can hold it."
                } else null
            } catch (e: Exception) {
                "Couldn't send: ${e.message}"
            }
            synchronized(pending) { pending.remove(item) }
            refresh()
            if (error != null) runOnUiThread {
                Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                if (compose.text.isEmpty()) compose.setText(text)
            }
        }
    }

    // ----- Send later -----

    /** Held Send: offer to send the message later. */
    private fun laterMenu(anchor: View) {
        if (compose.text.isBlank() || !::node.isInitialized) return
        PopupMenu(this, anchor).apply {
            menu.add("Send later…").setOnMenuItemClickListener { pickTime(null) { sendLater(it) }; true }
        }.show()
    }

    /** "14:05", "tomorrow 08:00", "Tue 08:00" or "12 Mar 08:00", in local time. */
    private fun whenText(ms: Long): String {
        val zone = java.time.ZoneId.systemDefault()
        val at = java.time.Instant.ofEpochMilli(ms).atZone(zone)
        val today = java.time.LocalDate.now(zone)
        val days = java.time.temporal.ChronoUnit.DAYS.between(today, at.toLocalDate())
        val locale = java.util.Locale.getDefault()
        val clock = time(ms)
        return when (days) {
            0L -> clock
            1L -> "tomorrow $clock"
            in 2L..6L -> at.format(java.time.format.DateTimeFormatter.ofPattern("EEE", locale)) + " " + clock
            else -> at.format(java.time.format.DateTimeFormatter.ofPattern(
                DateFormat.getBestDateTimePattern(locale, if (at.year == today.year) "dMMM" else "dMMMy"), locale)) + " " + clock
        }
    }

    /**
     * Asks when to send: in an hour, this evening (before 19:00), tomorrow
     * morning, or a date and time. Quick choices count from when one is
     * tapped. [current] is the time an item has now, if it has one.
     */
    private fun pickTime(current: Long?, done: (Long) -> Unit) {
        val zone = java.time.ZoneId.systemDefault()
        val now = java.time.ZonedDateTime.now(zone)
        val choices = mutableListOf<Pair<String, () -> java.time.ZonedDateTime>>()
        choices += "In 1 hour (${time(now.plusHours(1).toInstant().toEpochMilli())})" to {
            java.time.ZonedDateTime.now(zone).plusHours(1)
        }
        if (now.hour < 19) choices += "This evening, ${time(now.withHour(20).withMinute(0).toInstant().toEpochMilli())}" to {
            java.time.ZonedDateTime.now(zone).withHour(20).withMinute(0).withSecond(0).withNano(0)
        }
        val morning = { java.time.ZonedDateTime.now(zone).plusDays(1).withHour(8).withMinute(0).withSecond(0).withNano(0) }
        choices += "Tomorrow morning, ${whenText(morning().toInstant().toEpochMilli()).removePrefix("tomorrow ")} (" +
            morning().format(java.time.format.DateTimeFormatter.ofPattern("EEE", java.util.Locale.getDefault())) + ")" to morning
        val list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(8), 0, dp(8))
        }
        val dialog = SecureBuilder(this)
            .setTitle(if (current == null) "Send later" else "Change time")
            .setView(ScrollView(this).apply { addView(list) })
            .setNegativeButton("Cancel", null)
            .create()
        fun row(text: String, onClick: () -> Unit) = label(text, 16f, R.color.text).apply {
            setPadding(dp(24), dp(14), dp(24), dp(14))
            background = ripple()
            setOnClickListener { dialog.dismiss(); onClick() }
        }
        fun pick(due: Long) {
            if (due <= System.currentTimeMillis()) {
                Toast.makeText(this, "Pick a time in the future", Toast.LENGTH_LONG).show()
            } else done(due)
        }
        for ((text, at) in choices) list.addView(row(text) { pick(at().toInstant().toEpochMilli()) }, matchWrap)
        list.addView(row("Pick date and time…") { pickDateTime(current, ::pick) }, matchWrap)
        // Woken by pushes without exact alarms, it may send a few minutes late.
        if (Push.active(this) && !Scheduled.exactAllowed(this) && android.os.Build.VERSION.SDK_INT >= 31) {
            list.addView(row("May send a few minutes late. Allow exact timing") {
                try {
                    startActivity(Intent(android.provider.Settings.ACTION_REQUEST_SCHEDULE_EXACT_ALARM,
                        Uri.parse("package:$packageName")))
                } catch (_: Exception) {}
            }.apply { setTextColor(color(R.color.accent)); textSize = 14f; maxLines = 3 }, matchWrap)
        }
        list.addView(label("Sends from this device. Threnody needs to be running here (it wakes itself on Android).",
            13f, R.color.muted, maxLines = 4).apply { setPadding(dp(24), dp(12), dp(24), 0) }, matchWrap)
        dialog.show()
    }

    /** A date (today up to a year ahead), then a time. */
    private fun pickDateTime(current: Long?, done: (Long) -> Unit) {
        val zone = java.time.ZoneId.systemDefault()
        val start = java.time.Instant.ofEpochMilli(current ?: (System.currentTimeMillis() + 3_600_000)).atZone(zone)
        val now = System.currentTimeMillis()
        android.app.DatePickerDialog(this, { _, y, m, d ->
            android.app.TimePickerDialog(this, { _, h, min ->
                done(java.time.ZonedDateTime.of(y, m + 1, d, h, min, 0, 0, zone).toInstant().toEpochMilli())
            }, start.hour, start.minute, DateFormat.is24HourFormat(this)).apply { Privacy.secure(this) }.show()
        }, start.year, start.monthValue - 1, start.dayOfMonth).apply {
            datePicker.minDate = now
            datePicker.maxDate = now + 365L * 24 * 3_600_000
            Privacy.secure(this)
        }.show()
    }

    /**
     * Schedules what's in the compose field (with the reply and mentions)
     * for [due], as [send] sends it now. The field clears only once the
     * time is picked; on an error the text comes back.
     */
    private fun sendLater(due: Long) {
        val raw = compose.text.toString()
        val text = raw.trim()
        if (text.isEmpty() || !::node.isInitialized || note) return
        val mentions = if (group != null) Mentions.of(compose.text, raw.indexOf(text), text) else emptyList()
        val to = replyTo
        replyingTo(null)
        typingHandler.removeCallbacks(stopTyping)
        compose.setText("") // also says we stopped typing
        Threading.background {
            val g = group
            val error = try {
                if (g != null) node.scheduleGroupText(g, text, due.toULong(), to?.atMs, to?.device, mentions)
                else node.scheduleText(device, text, due.toULong(), to?.atMs, to?.device)
                null
            } catch (e: Exception) {
                e.message ?: "Couldn't schedule it"
            }
            refresh()
            runOnUiThread {
                if (error == null) {
                    Toast.makeText(this, "Scheduled for ${whenText(due)}", Toast.LENGTH_SHORT).show()
                } else {
                    Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                    if (compose.text.isEmpty()) compose.setText(text)
                }
            }
        }
    }

    /** What the scheduled bar says; hidden with nothing scheduled. */
    private fun showScheduled(list: List<ScheduledInfo>) {
        scheduled = list
        if (list.isEmpty()) {
            scheduledBar.visibility = View.GONE
            return
        }
        val failed = list.count { it.failed != null }
        val waiting = list.filter { it.failed == null }
        scheduledBar.text = buildString {
            append("🕓 ${list.size} scheduled")
            waiting.firstOrNull()?.let { append(" · next ${whenText(it.dueMs.toLong())}") }
            if (failed > 0) append(" · ⚠ $failed not sent")
        }
        scheduledBar.setTextColor(color(if (failed > 0) R.color.warning else R.color.accent))
        scheduledBar.contentDescription = "${scheduledBar.text}. Tap to see them."
        scheduledBar.visibility = View.VISIBLE
    }

    /** This chat's scheduled messages; tapping one offers what to do with it. */
    private fun scheduledList() {
        val items = scheduled
        if (items.isEmpty()) return
        val list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(8), 0, dp(8))
        }
        val dialog = SecureBuilder(this)
            .setTitle("Scheduled messages")
            .setView(ScrollView(this).apply { addView(list) })
            .setNegativeButton("Close", null)
            .create()
        for (item in items) {
            list.addView(LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(24), dp(10), dp(24), dp(10))
                background = ripple()
                addView(label(whenText(item.dueMs.toLong()) + if (item.quote != null) "  ↩ reply" else "",
                    13f, R.color.accent))
                addView(label(item.text, 15f, R.color.text).apply {
                    ellipsize = android.text.TextUtils.TruncateAt.END
                })
                item.failed?.let {
                    addView(label("Not sent: ${Scheduled.reasonText(it)}", 13f, R.color.warning, maxLines = 2).icon(R.drawable.ic_warning, 16))
                }
                if (item.sending) addView(label("Sending now…", 13f, R.color.muted))
                setOnClickListener {
                    dialog.dismiss()
                    if (item.sending) beingSent() else scheduledActions(item)
                }
            }, matchWrap)
        }
        if (timerOn) list.addView(label("Disappearing messages apply when it's sent.", 13f, R.color.muted, maxLines = 3)
            .apply { setPadding(dp(24), dp(8), dp(24), 0) }, matchWrap)
        dialog.show()
    }

    /** Send now, edit, move or delete one scheduled message. */
    private fun scheduledActions(item: ScheduledInfo) {
        SecureBuilder(this)
            .setTitle(whenText(item.dueMs.toLong()))
            .setItems(arrayOf("Send now", "Edit text", "Change time", "Delete")) { _, which ->
                when (which) {
                    0 -> Threading.background { runScheduled("send it") { node.sendScheduledNow(item.id) } }
                    1 -> editScheduled(item)
                    2 -> pickTime(item.dueMs.toLong()) { due ->
                        Threading.background {
                            runScheduled("change the time") { node.reschedule(item.id, due.toULong()) }
                        }
                    }
                    3 -> SecureBuilder(this)
                        .setMessage("Delete this scheduled message? It won't be sent.")
                        .setPositiveButton("Delete") { _, _ ->
                            Threading.background { runScheduled("delete it") { node.cancelScheduled(item.id) } }
                        }
                        .setNegativeButton("Cancel", null)
                        .show()
                }
            }
            .show()
    }

    private fun editScheduled(item: ScheduledInfo) {
        val field = EditText(this).apply {
            setText(item.text)
            setSelection(text.length)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES or
                InputType.TYPE_TEXT_FLAG_MULTI_LINE
        }
        SecureBuilder(this)
            .setTitle("Edit scheduled message")
            .setView(android.widget.FrameLayout(this).apply {
                setPadding(dp(20), dp(8), dp(20), 0)
                addView(field)
            })
            .setPositiveButton("Save") { _, _ ->
                val text = field.text.toString().trim()
                Threading.background { runScheduled("edit it") { node.editScheduled(item.id, text) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /**
     * What can be sent besides text: a sheet of round, coloured tiles (four
     * to a row), each a kind of thing to share.
     */
    @Suppress("UNUSED_PARAMETER")
    private fun attach(anchor: View) {
        val a = this
        val g = group
        val choices = buildList<Triple<String, Pair<Int, Int>, () -> Unit>> {
            add(Triple("Photos", R.drawable.ic_image to 0xFF7C4DFF.toInt()) { pick(photos = true) })
            add(Triple("File", R.drawable.ic_file to 0xFF3D7BF7.toInt()) { pick(photos = false) })
            add(Triple("Video note", R.drawable.ic_videocam to 0xFFE5484D.toInt()) { recordVideo() })
            add(Triple("GIF", R.drawable.ic_gif to 0xFF12A594.toInt()) { gifs() })
            add(Triple("Contact", R.drawable.ic_person to 0xFF0EA5E9.toInt()) { shareContact() })
            add(Triple("Location", R.drawable.ic_place to 0xFF22A559.toInt()) { Cards.composeLocation(a, LOCATION_PERMISSION) { sendCard(it) } })
            add(Triple("Event", R.drawable.ic_event to 0xFFF76B15.toInt()) { Cards.composeEvent(a) { sendCard(it) } })
            add(Triple("Wi-Fi", R.drawable.ic_wifi to 0xFF8E4EC6.toInt()) { Cards.composeWifi(a) { sendCard(it) } })
            // Secrets go to one person (or our own notes), never a group.
            if (g == null) add(Triple("Login or card", R.drawable.ic_lock to 0xFFD6409F.toInt()) {
                Cards.composeSecret(a, inNotes = note) { sendCard(it) }
            })
            if (g != null) add(Triple("Poll", R.drawable.ic_poll to 0xFFE5A000.toInt()) {
                Polls.compose(a) { q, options, multi ->
                    Threading.background { run("send the poll") { node.sendGroupPoll(g, q, options, multi) } }
                }
            })
        }
        lateinit var sheet: android.app.Dialog
        val grid = android.widget.GridLayout(this).apply {
            columnCount = 4
            setPadding(0, dp(Design.sm), 0, dp(Design.sm))
        }
        for ((name, look, onClick) in choices) {
            val (icon, tone) = look
            grid.addView(LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER_HORIZONTAL
                setPadding(0, dp(Design.md), 0, dp(Design.md))
                background = ripple()
                contentDescription = name
                addView(iconTile(icon, 56, android.graphics.Color.WHITE, tone))
                addView(label(name, 13f, R.color.text, maxLines = 2).apply {
                    gravity = Gravity.CENTER
                    setPadding(dp(2), dp(Design.sm), dp(2), 0)
                }, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                setOnClickListener { v ->
                    Design.lightHaptic(v)
                    sheet.dismiss()
                    onClick()
                }
            }, android.widget.GridLayout.LayoutParams(
                android.widget.GridLayout.spec(android.widget.GridLayout.UNDEFINED),
                android.widget.GridLayout.spec(android.widget.GridLayout.UNDEFINED, 1f),
            ).apply { width = 0 })
        }
        sheet = bottomSheet(grid)
        sheet.show()
    }

    /**
     * Photos, files, or (`gifs`) only the GIFs already on this phone: no
     * GIF service is asked. The keyboard's GIF search works too; see
     * [ComposeField].
     */
    private fun pick(photos: Boolean, gifs: Boolean = false) {
        val intent = if (photos && android.os.Build.VERSION.SDK_INT >= 33) {
            // The system photo picker: no storage permission needed.
            Intent(android.provider.MediaStore.ACTION_PICK_IMAGES)
                .putExtra(android.provider.MediaStore.EXTRA_PICK_IMAGES_MAX, MAX_PICK)
                .apply { if (gifs) type = "image/gif" }
        } else {
            Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                .setType(if (gifs) "image/gif" else if (photos) "image/*" else "*/*")
                .putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
        pickFileLauncher.launch(intent) { resultCode, data ->
            if (resultCode != RESULT_OK || data == null) return@launch
            val clip = data.clipData
            val uris = if (clip != null) (0 until clip.itemCount).map { clip.getItemAt(it).uri } else listOfNotNull(data.data)
            if (uris.isEmpty()) return@launch
            Threading.background {
                try {
                    val max = node.maxFileSize().toLong()
                    val offers = group == null && !note
                    val picked = uris.take(MAX_PICK).map { uri ->
                        // Keep access so a sent file can be opened from the chat later.
                        try { contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) } catch (e: Exception) {
                            Threnody.say("! persist uri permission: ${e.message}")
                        }
                        val (shown, size) = contentResolver.query(uri, null, null, null, null)?.use { c ->
                            c.moveToFirst()
                            c.getString(c.getColumnIndexOrThrow(OpenableColumns.DISPLAY_NAME)) to
                                c.getLong(c.getColumnIndexOrThrow(OpenableColumns.SIZE))
                        } ?: ("file" to -1L)
                        // A GIF must be named one to be shown animated.
                        val gif = contentResolver.getType(uri) == "image/gif" && !Media.isGif(shown)
                        val name = if (gif) shown.substringBeforeLast('.') + ".gif" else shown
                        // Too large for one message: offered in a chat with
                        // one contact, read in pieces where it is, never
                        // whole. A photo to convert is decoded from there.
                        if (size > max && offers) {
                            if (!Media.needsConverting(this, name, uri)) return@map Picked(name, null, uri, size)
                            val (n, d) = Media.converted(name, android.graphics.ImageDecoder.createSource(contentResolver, uri))
                            if (d.size > max) throw IllegalArgumentException("$n is too large")
                            return@map Picked(n, d, uri)
                        }
                        if (size > max * 2) throw IllegalArgumentException("$name is too large")
                        val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                            ?: throw IllegalArgumentException("couldn't read $name")
                        val (n, d) = Media.prepare(this, name, bytes)
                        if (d.size > max) {
                            throw IllegalArgumentException("$n is larger than " + Formatter.formatShortFileSize(this, max))
                        }
                        Picked(n, d, uri)
                    }
                    runOnUiThread { sendSheet(picked) }
                } catch (e: Exception) {
                    runOnUiThread { Toast.makeText(this@ChatActivity, "Couldn't send: ${e.message}", Toast.LENGTH_LONG).show() }
                }
            }
        }
    }

    /**
     * A GIF or sticker from the keyboard: into the send sheet, like a
     * picked photo.
     */
    private fun received(uri: Uri, mime: String, release: () -> Unit) {
        Threading.background {
            val picked = try {
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                    ?: throw IllegalArgumentException("unreadable")
                val ext = mime.substringAfter('/')
                val (n, d) = Media.prepare(this, "${if (ext == "gif") "gif" else "sticker"}-${System.currentTimeMillis()}.$ext", bytes)
                if (d.size > node.maxFileSize().toLong()) throw IllegalArgumentException("too large to send")
                Picked(n, d, uri)
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't add it: ${e.message}", Toast.LENGTH_LONG).show() }
                null
            } finally {
                release()
            }
            if (picked != null) runOnUiThread { sendSheet(listOf(picked)) }
        }
    }

    /**
     * GIPHY's GIFs once the user has agreed to GIPHY seeing their searches;
     * until then, or without an API key, the GIFs on this phone.
     */
    private fun gifs() {
        val phone = { pick(photos = true, gifs = true) }
        if (!Privacy.giphy(this)) {
            SecureBuilder(this)
                .setTitle("Search GIFs with GIPHY?")
                .setMessage("GIPHY will see what you search for and this phone's IP address. " +
                    "It won't see who you send GIFs to, and your contacts' phones never contact it. " +
                    "You can turn this off in Settings.")
                .setPositiveButton("Use GIPHY") { _, _ -> Privacy.setGiphy(this, true); gifs() }
                .setNegativeButton("GIFs on this phone") { _, _ -> phone() }
                .show()
            return
        }
        if (Giphy.key(this).isBlank()) return giphyKey { gifs() }
        GifPicker(this, phone) { g -> giphy(g) }.show()
    }

    /** Asks for a GIPHY API key (from developers.giphy.com), as this build has none. */
    private fun giphyKey(then: () -> Unit) {
        val field = EditText(this).apply {
            hint = "API key"
            wrapping(newlines = false, max = 1)
        }
        SecureBuilder(this)
            .setTitle("GIPHY API key")
            .setMessage("This copy of Threnody was built without one. Get a free key at developers.giphy.com.")
            .setView(android.widget.FrameLayout(this).apply {
                setPadding(dp(20), 0, dp(20), 0)
                addView(field)
            })
            .setPositiveButton("Save") { _, _ ->
                if (field.text.isNotBlank()) { Giphy.setKey(this, field.text.toString()); then() }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Downloads a GIF from GIPHY into the send sheet, like a picked photo. */
    private fun giphy(g: Giphy.Gif) {
        Toast.makeText(this, "Getting the GIF…", Toast.LENGTH_SHORT).show()
        Giphy.net.execute {
            try {
                val max = node.maxFileSize().toLong()
                val bytes = Giphy.get(g.full, max)
                val picked = Picked("gif-${g.id.ifBlank { System.currentTimeMillis().toString() }}.gif", bytes, Uri.EMPTY)
                runOnUiThread { sendSheet(listOf(picked)) }
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't get the GIF: ${e.message}", Toast.LENGTH_LONG).show() }
            }
        }
    }

    /**
     * A picked file, read and ready to send; or (no `data`) one too large
     * for one message, offered from where it is.
     */
    private class Picked(val name: String, val data: ByteArray?, val uri: Uri, val size: Long = data?.size?.toLong() ?: 0)

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (!ActivityResultRegistry.dispatch(this, requestCode, resultCode, data)) {
            super.onActivityResult(requestCode, resultCode, data)
        }
    }

    /** Previews what's about to go, with a caption and the sensitive choice. */
    private fun sendSheet(picked: List<Picked>) {
        val images = picked.count { Media.isImage(it.name) }
        val strip = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for ((i, p) in picked.withIndex()) {
            val data = p.data
            val cell = if (data == null) {
                if (Media.isImage(p.name)) ImageView(this).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                    clipToOutline = true
                    contentDescription = p.name
                    Threading.background {
                        val b = Media.decode(android.graphics.ImageDecoder.createSource(contentResolver, p.uri), dp(200))
                        runOnUiThread { setImageBitmap(b) }
                    }
                } else label("${p.name}\n${Formatter.formatShortFileSize(this, p.size)}", 12f, R.color.text, maxLines = 4).apply {
                    gravity = Gravity.CENTER
                    icon(R.drawable.ic_file, 28, top = true)
                    setPadding(dp(6), dp(6), dp(6), dp(6))
                    background = rounded(color(R.color.card), dp(12).toFloat())
                }
            } else if (Media.isGif(p.name)) {
                ImageView(this).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = rounded(color(R.color.surface), dp(8).toFloat())
                    clipToOutline = true
                    contentDescription = "GIF"
                    Media.animated({ android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(data)) }, dp(200)) { d ->
                        runOnUiThread { setImageDrawable(d); (d as? android.graphics.drawable.AnimatedImageDrawable)?.start() }
                    }
                }
            } else if (Media.isImage(p.name)) {
                // The photo, with an Edit button on it: drawing and text.
                android.widget.FrameLayout(this).apply {
                    contentDescription = "${p.name}. Edit: draw or add text"
                    setOnClickListener { annotate(picked, i) }
                    addView(ImageView(this@ChatActivity).apply {
                        scaleType = ImageView.ScaleType.CENTER_CROP
                        background = rounded(color(R.color.surface), dp(8).toFloat())
                        clipToOutline = true
                        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                        Threading.background {
                            val b = Media.decode(android.graphics.ImageDecoder.createSource(java.nio.ByteBuffer.wrap(data)), dp(200))
                            runOnUiThread { setImageBitmap(b) }
                        }
                    }, MATCH_PARENT, MATCH_PARENT)
                    addView(TextView(this@ChatActivity).apply {
                        text = "✏️ Edit"
                        textSize = 12f
                        setTypeface(typeface, Typeface.BOLD)
                        setTextColor(android.graphics.Color.WHITE)
                        background = rounded(android.graphics.Color.argb(170, 0, 0, 0), dp(12).toFloat())
                        setPadding(dp(8), dp(3), dp(8), dp(3))
                        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                    }, android.widget.FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.BOTTOM or Gravity.END).apply {
                        setMargins(dp(4), dp(4), dp(4), dp(4))
                    })
                }
            } else {
                label(p.name, 12f, R.color.text, maxLines = 3).apply {
                    gravity = Gravity.CENTER
                    icon(R.drawable.ic_file, 28, top = true)
                    setPadding(dp(6), dp(6), dp(6), dp(6))
                    background = rounded(color(R.color.card), dp(12).toFloat())
                }
            }
            strip.addView(cell, LinearLayout.LayoutParams(dp(104), dp(104)).apply { marginEnd = dp(6) })
        }
        val caption = EditText(this).apply {
            hint = "Add a message"
            setText(compose.text)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            wrapping(max = 6)
        }
        val sensitive = android.widget.CheckBox(this).apply {
            text = if (images > 0) "Sensitive: they see it covered until they tap it" else "Sensitive: shown covered until opened"
        }
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), 0)
            addView(android.widget.HorizontalScrollView(this@ChatActivity).apply { addView(strip) }, matchWrap)
            addView(caption, matchWrap)
            addView(sensitive, matchWrap)
            if (images > 0 && Privacy.stripMetadata(this@ChatActivity)) {
                addView(label("Location and camera details are removed before sending.", 12f, R.color.muted, maxLines = 0), matchWrap)
            }
        }
        val gifs = picked.count { Media.isGif(it.name) }
        val what = when {
            gifs == picked.size -> if (gifs == 1) "1 GIF" else "$gifs GIFs"
            images == picked.size -> if (images == 1) "1 photo" else "$images photos"
            picked.size == 1 -> "1 file"
            else -> "${picked.size} files"
        }
        sheet = Sheet(picked, caption, sensitive, SecureBuilder(this)
            .setTitle("Send $what")
            .setView(android.widget.ScrollView(this).apply { addView(body) })
            .setPositiveButton("Send") { _, _ ->
                compose.setText("")
                sendFiles(picked, caption.text.toString().trim(), sensitive.isChecked)
            }
            .setNegativeButton("Cancel", null)
            .setOnDismissListener { if (sheet?.annotating != true) sheet = null }
            .show())
    }

    /** The open send sheet, kept while one of its photos is drawn on. */
    private class Sheet(
        val picked: List<Picked>,
        val caption: EditText,
        val sensitive: android.widget.CheckBox,
        val dialog: AlertDialog,
        var annotating: Boolean = false,
    )
    private var sheet: Sheet? = null

    /** Opens picked photo `i` for editing; the sheet comes back with the result. */
    private fun annotate(picked: List<Picked>, i: Int) {
        val s = sheet ?: return
        val p = picked[i]
        val data = p.data ?: return
        Threading.background {
            val f = java.io.File(java.io.File(cacheDir, AnnotateActivity.DIR).apply { mkdirs() }, "source-$i")
            try { f.writeBytes(data) } catch (_: Exception) { return@background }
            runOnUiThread {
                s.annotating = true
                s.dialog.dismiss()
                annotateLauncher.launch(
                    Intent(this, AnnotateActivity::class.java)
                        .putExtra(AnnotateActivity.LOCATION, Uri.fromFile(f).toString())
                        .putExtra(AnnotateActivity.NAME, p.name)
                        .putExtra(AnnotateActivity.INDEX, i)
                ) { resultCode, data ->
                    annotated(if (resultCode == RESULT_OK) data else null)
                }
            }
        }
    }

    /** A photo edited from the viewer: ready to send, in the send sheet, *Sensitive* as the original was. */
    private fun sendEdited(path: String, name: String, sensitive: Boolean) {
        Threading.background {
            val f = java.io.File(path)
            val bytes = try { f.readBytes() } catch (_: Exception) { null }
            java.io.File(cacheDir, AnnotateActivity.DIR).deleteRecursively()
            if (bytes != null) runOnUiThread {
                sendSheet(listOf(Picked(name.substringBeforeLast('.') + "-edited.jpg", bytes, Uri.fromFile(f))))
                sheet?.sensitive?.isChecked = sensitive
            }
        }
    }

    /** An edited photo came back: it replaces the picked one in the sheet. */
    private fun annotated(data: Intent?) {
        val s = sheet ?: return
        val i = data?.getIntExtra(AnnotateActivity.INDEX, -1) ?: -1
        val path = data?.getStringExtra(AnnotateActivity.RESULT_PATH)
        compose.setText(s.caption.text)
        val sensitive = s.sensitive.isChecked
        Threading.background {
            val bytes = if (path == null || i !in s.picked.indices) null
                else try { java.io.File(path).readBytes() } catch (_: Exception) { null }
            java.io.File(cacheDir, AnnotateActivity.DIR).deleteRecursively()
            runOnUiThread {
                val picked = if (bytes == null) s.picked else s.picked.toMutableList().also {
                    val old = it[i]
                    it[i] = Picked(old.name.substringBeforeLast('.') + ".jpg", bytes, old.uri)
                }
                sendSheet(picked)
                sheet?.sensitive?.isChecked = sensitive
            }
        }
    }

    /** Forwards messages (a whole album) to a chat the user picks, marked as forwarded. */
    private fun forward(entries: List<HistoryEntry>) {
        Threading.background {
            data class Target(val title: String, val send: (HistoryEntry) -> Unit)
            val targets = buildList {
                add(Target("Note to self") { e ->
                    val f = e.file
                    if (f == null) node.writeNote(e.text, null, null)
                    else node.addNoteFile(f.name, f.size, copyOf(f).second,
                        FileOptions(f.sensitive, e.text, 0uL, f.clip, true))
                })
                for (c in Threnody.conversations(node, persona).filter { it.accepted && !it.blocked }) {
                    add(Target(c.title) { e ->
                        val f = e.file
                        if (f == null) node.forwardText(c.device, e.text)
                        else copyOf(f).let { (data, loc) ->
                            node.sendFile(c.device, f.name, data, loc, FileOptions(f.sensitive, e.text, 0uL, f.clip, true))
                        }
                    })
                }
                for (gi in node.groups()) {
                    add(Target(gi.name) { e ->
                        val f = e.file
                        if (f == null) node.forwardGroupText(gi.id, e.text)
                        else copyOf(f).let { (data, loc) ->
                            node.sendGroupFile(gi.id, f.name, data, loc, FileOptions(f.sensitive, e.text, 0uL, f.clip, true))
                        }
                    })
                }
            }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle("Forward to")
                    .setItems(targets.map { it.title }.toTypedArray()) { _, i ->
                        Threading.background {
                            run("forward") { entries.forEach { targets[i].send(it) } }
                            runOnUiThread { Toast.makeText(this, "Forwarded to ${targets[i].title}", Toast.LENGTH_SHORT).show() }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    /**
     * A file's bytes, and where the forwarded copy is kept: a private copy,
     * so deleting one message never takes another's file.
     */
    private fun copyOf(f: uniffi.threnody_ffi.FileInfo): Pair<ByteArray, String?> {
        if (f.size > node.maxFileSize()) throw IllegalStateException("files this large can't be forwarded yet")
        val data = readFile(f.location)
        return data to (Media.savePrivate(this, f.name, data, persona) ?: f.location)
    }

    /** A received or sent file's bytes, from where it was kept. */
    private fun readFile(location: String?): ByteArray {
        val l = location ?: throw IllegalStateException("that file isn't on this device")
        return Media.read(this, l, Long.MAX_VALUE) ?: throw IllegalStateException("that file is gone")
    }

    /** Keeps what's in the compose field, encrypted, for next time. */
    private fun saveDraft() {
        if (!::node.isInitialized || !draftShown) return
        val text = compose.text.toString()
        val g = group
        val d = device
        Threading.background {
            try {
                when {
                    note -> node.setNoteDraft(text)
                    g != null -> node.setGroupDraft(g, text)
                    else -> node.setDraft(d, text)
                }
            } catch (_: Exception) {}
        }
    }

    /** Sends a file here: to the group, the contact, or into the notes to self. */
    private fun sendFileTo(name: String, data: ByteArray, location: String?, options: FileOptions) {
        val g = group
        when {
            note -> node.addNoteFile(name, data.size.toULong(), location, options)
            g != null -> node.sendGroupFile(g, name, data, location, options)
            else -> node.sendFile(device, name, data, location, options)
        }
    }

    private fun sendFiles(picked: List<Picked>, caption: String, sensitive: Boolean) {
        val album = if (picked.size > 1) uniffi.threnody_ffi.albumId() else 0uL
        Threading.background {
            val g = group
            var failed: String? = null
            for ((i, p) in picked.withIndex()) {
                val options = FileOptions(sensitive, if (i == 0) caption else "", album)
                val data = p.data
                // Keep our own copy of a photo privately, so the chat can
                // show it; a large file stays where it is, and is read
                // from there whenever they download it.
                val location = if (data != null && Media.isImage(p.name)) Media.savePrivate(this, p.name, data, persona) else p.uri.toString()
                try {
                    if (data != null) sendFileTo(p.name, data, location, options)
                    else {
                        runOnUiThread { Toast.makeText(this, "Preparing ${p.name}…", Toast.LENGTH_SHORT).show() }
                        val at = location ?: p.uri.toString()
                        // Shown in their chat before they download it; never
                        // for a sensitive file, which stays covered there.
                        val thumbnail = if (sensitive) null else Media.offerThumbnail(this, p.name, at)
                        node.sendFileAt(device, p.name, at, options.copy(thumbnail = thumbnail))
                    }
                } catch (e: Exception) {
                    Media.forget(this, location)
                    failed = e.message
                }
                refresh()
            }
            if (failed != null) runOnUiThread {
                Toast.makeText(this, "Couldn't send: $failed", Toast.LENGTH_LONG).show()
            }
        }
    }

    private fun open(location: Uri) {
        val uri = FilesProvider.shareable(this, location)
        try {
            startActivity(Intent(Intent.ACTION_VIEW, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        } catch (_: Exception) {
            Toast.makeText(this, "No app can open this file", Toast.LENGTH_SHORT).show()
        }
    }

    private fun more(anchor: View) {
        if (group != null) return groupMenu(anchor)
        if (note) return search.open()
        val c = convo
        PopupMenu(this, anchor).apply {
            val a = this@ChatActivity
            item(a, "Search", R.drawable.ic_search) { search.open() }
            item(a, "Media, files and links", R.drawable.ic_image) { media(c?.title) }
            item(a, "Disappearing messages", R.drawable.ic_timer) { disappearing() }
            item(a, "Notifications", R.drawable.ic_notifications) { notifications() }
            item(a, "Chat theme", R.drawable.ic_image) { chooseTheme() }
            item(a, "Contact details", R.drawable.ic_info) { details() }
            show()
        }
    }

    private fun media(title: String?) {
        startActivity(Intent(this, MediaActivity::class.java)
            .putExtra(DEVICE, device)
            .putExtra(GROUP, group)
            .putExtra(PERSONA, persona)
            .putExtra(MediaActivity.TITLE, title))
    }

    /** Proves to them that this anonymous identity is us. Can't be undone. */
    private fun reveal(c: Conversation) {
        val p = persona ?: return
        SecureBuilder(this)
            .setTitle("Reveal who you are?")
            .setMessage(
                "${c.title} will get proof, signed by your main identity, that this anonymous identity is you, " +
                    "and an invite to reach you. They can show that proof to anyone. This can't be taken back.",
            )
            .setPositiveButton("Reveal") { _, _ ->
                Threading.background {
                    val main = Threnody.start(this)
                    val error = try {
                        val p2 = Threnody.personaNode(p) ?: throw IllegalStateException("anonymous identity is gone")
                        main.revealThrough(p2, device, mainInvite(main))
                        null
                    } catch (e: Exception) {
                        e.message
                    }
                    runOnUiThread {
                        Toast.makeText(this, error?.let { "Couldn't reveal: $it" } ?: "${c.title} now knows who you are",
                            Toast.LENGTH_LONG).show()
                    }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Sends a card here (Appendix V): to the contact, the group, or our notes. */
    private fun sendCard(card: CardInfo) {
        Threading.background {
            val g = group
            val error = try {
                when {
                    note -> node.writeNoteCard(card)
                    g != null -> node.sendGroupCard(g, card)
                    node.sendCard(device, card) == 0u -> throw Exception("they're offline and no mutual contact can hold it")
                }
                null
            } catch (e: Exception) {
                e.message ?: "it didn't go"
            }
            refresh()
            if (error != null) runOnUiThread { Toast.makeText(this, "Not sent: $error", Toast.LENGTH_LONG).show() }
        }
    }

    /** Opens a card in full: a secret's fields, or a view-once card (which empties it). */
    private fun openCard(e: HistoryEntry) {
        Threading.background {
            val g = group
            val c = try {
                when {
                    note -> node.openNoteCard(e.atMs, e.device)
                    g != null -> node.openGroupCard(g, e.atMs, e.device)
                    else -> node.openCard(device, e.atMs, e.device)
                }
            } catch (_: Exception) { null } ?: return@background
            runOnUiThread {
                Cards.show(this, c) { addFromCard(it) }
                if (c.viewOnce) refreshSoon()
            }
        }
    }

    /** Adds the person a contact card names, by its invite link or handle. */
    private fun addFromCard(link: String) {
        Threading.background {
            val error = try { node.connect(link); null } catch (e: Exception) { e.message }
            runOnUiThread {
                Toast.makeText(this, error?.let { "Couldn't reach them: $it" } ?: "Added. They're in your conversations.",
                    Toast.LENGTH_LONG).show()
            }
        }
    }

    /** Shares a contact: ourselves (with this identity's invite), one from the phone's contacts, or one typed in. */
    private fun shareContact() {
        val options = arrayOf("My contact", "From your contacts", "Type one in")
        SecureBuilder(this)
            .setTitle("Share a contact")
            .setItems(options) { _, i ->
                when (i) {
                    0 -> Threading.background {
                        val addr = Threnody.inviteAddr(this, Threnody.listenAddr.substringAfterLast(':'))
                        val invite = if (addr.isEmpty() && !node.reachInternet()) "" else node.inviteLink(addr)
                        val handle = try { node.ownHandle() } catch (_: Exception) { null } ?: ""
                        val name = try { node.profile().firstOrNull { it.key == "name" }?.value } catch (_: Exception) { null } ?: ""
                        runOnUiThread { Cards.composeContact(this, name, "", "", invite, handle) { sendCard(it) } }
                    }
                    1 -> pickFileLauncher.launch(Intent(Intent.ACTION_PICK, android.provider.ContactsContract.CommonDataKinds.Phone.CONTENT_URI)) { code, data ->
                        val uri = data?.data
                        if (code != RESULT_OK || uri == null) return@launch
                        // The pick grants reading this one entry; no contacts permission.
                        val (name, phone) = contentResolver.query(uri, arrayOf(
                            android.provider.ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME,
                            android.provider.ContactsContract.CommonDataKinds.Phone.NUMBER,
                        ), null, null, null)?.use { c ->
                            if (c.moveToFirst()) (c.getString(0) ?: "") to (c.getString(1) ?: "") else "" to ""
                        } ?: ("" to "")
                        Cards.composeContact(this, name, phone, "", "", "") { sendCard(it) }
                    }
                    else -> Cards.composeContact(this, "", "", "", "", "") { sendCard(it) }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Our main identity's invite, on this device's current address. */
    private fun mainInvite(main: ThrenodyNode): String? {
        val addr = Threnody.inviteAddr(this, Threnody.listenAddr.substringAfterLast(':'))
        if (addr.isEmpty() && !main.reachInternet()) return null
        return main.inviteLink(addr)
    }

    /** Adds the identity they revealed, as a contact of our main identity. */
    private fun addRevealed(invite: String) {
        Threading.background {
            val error = try { Threnody.start(this).connect(invite); null } catch (e: Exception) { e.message }
            runOnUiThread {
                Toast.makeText(this, error?.let { "Couldn't reach them: $it" } ?: "Added. They're in your conversations.",
                    Toast.LENGTH_LONG).show()
            }
            refresh()
        }
    }

    /**
     * Everything about this contact that the chat's menu used to list:
     * who they are, trust, what you share with them, and at the bottom,
     * clearing and deleting.
     */
    private fun details() {
        val c = convo ?: return
        val who = c.title
        SettingsPage(this, who).apply {
            section("Contact")
            link("Rename", "Only you see the name you give them") { rename() }
            link("Safety number", "Compare it with them to be sure it's them") { safety() }
            toggle("Approved", { convo?.approved == true }, {
                if (it) "Your devices find each other nearby and over the internet, relay for each other, " +
                    "and can hold each other's messages"
                else "Only chat: no relaying, tunnels or messages held for them"
            }) { setApproval(it) }

            section("Share")
            link("Share your profile\u2026", "Choose which details $who sees") {
                ProfileUi.share(this@ChatActivity, node, device, who,
                    if (persona != null) "This identity's profile" else "Your profile")
            }
            link("Offer a credential\u2026", "Vouch for something about them") {
                CredentialUi.offer(this@ChatActivity, node, device, who)
            }
            link("Ask for a credential\u2026", "Ask them to prove something") {
                CredentialUi.ask(this@ChatActivity, node, device, who)
            }
            if (persona != null) {
                link("Reveal who you are\u2026", "Prove this anonymous identity is you. Can't be undone") { reveal(c) }
            }
            // Not for an anonymous identity: a group shows a nearby contact the phone.
            if (persona == null && c.connected && c.approved) {
                section("Connection")
                link("Faster link (Wi-Fi Direct)", "For big files when you're near each other") { wifiDirect() }
            }

            section("Delete")
            link("Clear chat\u2026", "Deletes the messages; keeps the contact", danger = true) {
                Chats.clear(this@ChatActivity, node, who, c.device, null) { refresh() }
            }
            link("Delete contact\u2026", danger = true) {
                Chats.delete(this@ChatActivity, node, who, c.device) { finish() }
            }
        }.show()
    }

    private fun groupMenu(anchor: View) {
        val g = info ?: return
        PopupMenu(this, anchor).apply {
            val a = this@ChatActivity
            item(a, "Search", R.drawable.ic_search) { search.open() }
            item(a, "Media, files and links", R.drawable.ic_image) { media(g.name) }
            item(a, "Members", R.drawable.ic_group) { members() }
            if (g.myRole != MemberRole.MEMBER) item(a, "Invite contacts", R.drawable.ic_add) { inviteToGroup() }
            item(a, "Disappearing messages", R.drawable.ic_timer) { disappearing() }
            item(a, "Group theme", R.drawable.ic_image) { chooseTheme() }
            item(a, "Clear chat…", R.drawable.ic_delete) { Chats.clear(a, node, g.name, null, g.id) { refresh() } }
            if (g.owned) item(a, "Delete group", R.drawable.ic_delete) { deleteGroup() }
            item(a, "Leave group", R.drawable.ic_back) { leaveGroup() }
            show()
        }
    }

    /** The members, one row per account, with what we may do to each. */
    private fun members() {
        val g = info ?: return
        Threading.background {
            val rows = try { node.groupMemberRows(g.id) } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Couldn't list members: ${e.message}", Toast.LENGTH_LONG).show() }
                return@background
            }
            val labels = rows.map { r ->
                val name = if (r.isMe) "You" else Threnody.nameOf(node, r.fingerprint)
                name + when (r.role) {
                    MemberRole.OWNER -> " (owner)"
                    MemberRole.ADMIN -> " (admin)"
                    else -> ""
                } + if (r.successor) " (successor)" else ""
            }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle(members(g.members.size))
                    .setItems(labels.toTypedArray()) { _, i -> if (rows[i].actions.isNotEmpty()) memberActions(rows[i], labels[i]) }
                    .setNegativeButton("Close", null)
                    .show()
            }
        }
    }

    private fun actionLabel(a: GroupMemberAction): String = when (a) {
        GroupMemberAction.MAKE_ADMIN -> "Make admin"
        GroupMemberAction.REMOVE_ADMIN -> "Remove as admin"
        GroupMemberAction.MAKE_SUCCESSOR -> "Make successor"
        GroupMemberAction.NO_LONGER_SUCCESSOR -> "No longer successor"
        GroupMemberAction.MAKE_OWNER -> "Make owner"
        GroupMemberAction.USE_AS_COORDINATOR -> "Apply changes from this device"
        GroupMemberAction.REMOVE -> "Remove from group"
        GroupMemberAction.REMOVE_THIS_DEVICE -> "Remove the lost coordinating device"
        GroupMemberAction.STEP_DOWN -> "Step down as admin"
    }

    /** What a confirm says before each action. */
    private fun actionConfirm(a: GroupMemberAction, name: String, group: String): String = when (a) {
        GroupMemberAction.MAKE_ADMIN -> "$name can invite and remove members and pin messages. " +
            "Members will see which devices are yours and $name's."
        GroupMemberAction.REMOVE_ADMIN -> "$name stays in $group as a member."
        GroupMemberAction.MAKE_SUCCESSOR -> "If you're not seen in $group for 14 days, $name may take it over."
        GroupMemberAction.NO_LONGER_SUCCESSOR -> "Nobody will be able to take $group over if you disappear."
        GroupMemberAction.MAKE_OWNER -> "$name owns $group from now on. You become an admin and can't undo this."
        GroupMemberAction.USE_AS_COORDINATOR -> "Changes to $group are applied from this device from now on."
        GroupMemberAction.REMOVE -> "They stop receiving new messages in $group. They can be invited again later."
        GroupMemberAction.REMOVE_THIS_DEVICE -> "This device takes over applying changes and removes your lost " +
            "device from $group."
        GroupMemberAction.STEP_DOWN -> "You stay in $group as a member."
    }

    private fun memberActions(r: GroupMemberRow, label: String) {
        val g = info ?: return
        val name = if (r.isMe) "You" else Threnody.nameOf(node, r.fingerprint)
        SecureBuilder(this)
            .setTitle(label)
            .setItems(r.actions.map { actionLabel(it) }.toTypedArray()) { _, i ->
                val a = r.actions[i]
                SecureBuilder(this)
                    .setTitle(actionLabel(a) + "?")
                    .setMessage(actionConfirm(a, name, g.name))
                    .setPositiveButton(actionLabel(a)) { _, _ ->
                        Threading.background {
                            run(actionLabel(a).lowercase()) {
                                when (a) {
                                    GroupMemberAction.MAKE_ADMIN -> node.promoteInGroup(g.id, r.fingerprint)
                                    GroupMemberAction.REMOVE_ADMIN, GroupMemberAction.STEP_DOWN ->
                                        node.demoteInGroup(g.id, r.fingerprint)
                                    GroupMemberAction.MAKE_SUCCESSOR -> node.setGroupSuccessor(g.id, r.fingerprint)
                                    GroupMemberAction.NO_LONGER_SUCCESSOR -> node.setGroupSuccessor(g.id, null)
                                    GroupMemberAction.MAKE_OWNER -> node.transferGroupOwnership(g.id, r.fingerprint)
                                    GroupMemberAction.USE_AS_COORDINATOR ->
                                        node.useAsCoordinator(g.id, node.deviceFingerprint())
                                    GroupMemberAction.REMOVE -> node.removeFromGroup(g.id, r.fingerprint)
                                    GroupMemberAction.REMOVE_THIS_DEVICE -> node.removeLostCoordinator(g.id)
                                }
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
            .setNegativeButton("Close", null)
            .show()
    }

    private fun inviteToGroup() {
        val g = info ?: return
        Threading.background {
            val candidates = Threnody.conversations(node, persona).filter { c -> c.devices.none { it in g.members } }
            runOnUiThread {
                if (candidates.isEmpty()) {
                    Toast.makeText(this, "All your contacts are already in this group", Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                val chosen = BooleanArray(candidates.size)
                SecureBuilder(this)
                    .setTitle("Invite to ${g.name}")
                    .setMultiChoiceItems(candidates.map { it.title }.toTypedArray(), chosen) { _, i, on -> chosen[i] = on }
                    .setPositiveButton("Invite") { _, _ ->
                        val picked = candidates.filterIndexed { i, _ -> chosen[i] }
                        Threading.background {
                            for (c in picked) run("invite ${c.title}") { node.inviteToGroup(g.id, c.device) }
                            if (picked.any { !it.approved }) runOnUiThread {
                                Toast.makeText(this, "Contacts who haven't approved you are asked before they join.",
                                    Toast.LENGTH_LONG).show()
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun leaveGroup() {
        val g = info ?: return
        Threading.background {
            // The last owner account, with others in the group: hand it over first.
            val rows = try { node.groupMemberRows(g.id) } catch (_: Exception) { emptyList() }
            val lastOwner = g.owned && rows.none { !it.isMe && it.role == MemberRole.OWNER } &&
                rows.any { !it.isMe }
            runOnUiThread {
                if (lastOwner) {
                    chooseNewOwner(g)
                    return@runOnUiThread
                }
                SecureBuilder(this)
                    .setTitle("Leave ${g.name}?")
                    .setMessage("You stop receiving its messages. Its history stays on your devices; " +
                        "it can invite you again.")
                    .setPositiveButton("Leave") { _, _ ->
                        Threading.background {
                            run("leave") { node.leaveGroup(g.id) }
                            runOnUiThread { finish() }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    /** The last owner leaving: someone else owns the group first. */
    private fun chooseNewOwner(g: GroupInfo) {
        Threading.background {
            val rows = try { node.groupMemberRows(g.id).filter { !it.isMe } } catch (_: Exception) { emptyList() }
            // Admins first.
            val sorted = rows.sortedBy { if (it.role == MemberRole.ADMIN) 0 else 1 }
            val labels = sorted.map { Threnody.nameOf(node, it.fingerprint) + if (it.role == MemberRole.ADMIN) " (admin)" else "" }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle("Choose a new owner first")
                    .setItems(labels.toTypedArray()) { _, i ->
                        val r = sorted[i]
                        SecureBuilder(this)
                            .setTitle("Make ${labels[i]} the owner and leave?")
                            .setMessage("${g.name} goes on without you. This can't be undone.")
                            .setPositiveButton("Leave") { _, _ ->
                                Threading.background {
                                    // Nothing changes if it can't go through
                                    // yet (members still to update, say).
                                    var done = false
                                    run("hand over the group") {
                                        node.transferGroupOwnershipAndLeave(g.id, r.fingerprint)
                                        done = true
                                    }
                                    if (done) runOnUiThread { finish() }
                                }
                            }
                            .setNegativeButton("Cancel", null)
                            .show()
                    }
                    .setNeutralButton("Delete instead") { _, _ -> deleteGroup() }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun deleteGroup() {
        val g = info ?: return
        SecureBuilder(this)
            .setTitle("Delete ${g.name}?")
            .setMessage("Everyone is removed and the group ends. Its history stays on your devices.")
            .setPositiveButton("Delete") { _, _ ->
                Threading.background {
                    run("delete the group") { node.deleteGroup(g.id) }
                    runOnUiThread { finish() }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Answers a message request: accept, block or delete. */
    private fun request(what: String) {
        Threading.background {
            run(what) {
                when (what) {
                    "accept" -> node.acceptContact(device)
                    "block" -> node.blockContact(device)
                    else -> node.deleteRequest(device)
                }
            }
            if (what != "accept") runOnUiThread { finish() }
        }
    }

    private fun declineGroup() {
        val g = group ?: return
        // Only the invitation shown: someone else may invite us to the same group.
        val from = shownInvite?.from
        Threading.background {
            run("decline") { if (from != null) node.declineGroupInviteFrom(g, from) else node.declineGroupInvite(g) }
            runOnUiThread { finish() }
        }
    }

    private fun joinGroup() {
        val g = group ?: return
        val from = shownInvite?.from
        Threading.background {
            run("join") { if (from != null) node.acceptGroupInviteFrom(g, from) else node.acceptGroupInvite(g) }
        }
    }

    private fun rename() {
        val field = EditText(this).apply {
            setText(convo?.name ?: "")
            hint = "Name"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS
            wrapping(newlines = false, max = 3)
        }
        SecureBuilder(this)
            .setTitle("Name this contact")
            .setMessage("Only you see this name.")
            .setView(LinearLayout(this).apply { setPadding(dp(24), dp(8), dp(24), 0); addView(field, matchWrap) })
            .setPositiveButton("Save") { _, _ ->
                val name = field.text.toString().trim()
                if (name.isNotEmpty()) Threading.background { run("rename") { node.setName(device, name) } }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Compares safety numbers, device by device: an unverified one first. */
    private fun safety() {
        val target = convo?.unverified?.firstOrNull() ?: device
        val many = (convo?.devices?.size ?: 1) > 1
        Threading.background {
            val number = try { node.safetyNumber(target) } catch (e: Exception) { return@background }
            // Twelve groups of five digits, three per line.
            val pretty = number.filter { it.isDigit() }.chunked(5).chunked(3).joinToString("\n") { it.joinToString("  ") }
            runOnUiThread {
                val view = label(pretty, 20f).apply {
                    typeface = Typeface.MONOSPACE
                    gravity = Gravity.CENTER
                    setPadding(dp(24), dp(16), dp(24), 0)
                }
                SecureBuilder(this)
                    .setTitle(if (many) "Safety number: device ${Threnody.short(target)}" else "Safety number")
                    .setMessage("Compare this with the number on ${convo?.title?.let { "$it's" } ?: "their"} " +
                        (if (many) "device ${Threnody.short(target)} " else "") +
                        "screen, in person or on a call. If they match, no one is intercepting your messages.")
                    .setView(view)
                    .setPositiveButton("They match") { _, _ ->
                        Threading.background {
                            run("verify") { node.markVerified(target) }
                            // More devices to check? Offer the next one.
                            val more = Threnody.conversations(node, persona).firstOrNull { it.key == key }?.unverified?.isNotEmpty() == true
                            if (more) runOnUiThread { safety() }
                        }
                    }
                    .setNegativeButton("Not now", null)
                    .show()
            }
        }
    }

    private fun setApproval(approved: Boolean) {
        val apply = { Threading.background { run(if (approved) "approve" else "revoke") { node.setApproval(device, approved) } } }
        if (approved) return apply()
        SecureBuilder(this)
            .setTitle("Revoke approval?")
            .setMessage("${convo?.title ?: "They"} will no longer be able to find you nearby, relay for you or hold your messages.")
            .setPositiveButton("Revoke") { _, _ -> apply() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Sets this conversation's disappearing timer (any of the choices, or off). */
    /** When this chat notifies: always, only for mentions and replies (groups), or never. */
    private fun notifications() {
        val g = group
        val levels = if (g != null) listOf(NotifyLevel.ALL, NotifyLevel.MENTIONS, NotifyLevel.OFF)
        else listOf(NotifyLevel.ALL, NotifyLevel.OFF)
        val names = levels.map {
            when (it) {
                NotifyLevel.ALL -> "All messages"
                NotifyLevel.MENTIONS -> "Only mentions and replies to me"
                NotifyLevel.OFF -> "Off"
            }
        }
        Threading.background {
            val current = try {
                if (g != null) node.groupNotifyLevel(g) else node.notifyLevel(device)
            } catch (_: Exception) { NotifyLevel.ALL }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle("Notifications")
                    .setSingleChoiceItems(names.toTypedArray(), levels.indexOf(current).coerceAtLeast(0)) { d, i ->
                        d.dismiss()
                        Threading.background {
                            run("change notifications") {
                                if (g != null) node.setGroupNotifyLevel(g, levels[i]) else node.setNotifyLevel(device, levels[i])
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    private fun disappearing() {
        val choices = Privacy.TIMERS
        Threading.background {
            val g = group
            val current = try {
                if (g != null) node.groupDisappearing(g) else node.disappearing(device)
            } catch (_: Exception) { null }
            val checked = choices.indexOfFirst { it.second == current }
            runOnUiThread {
                SecureBuilder(this)
                    .setTitle("Disappearing messages")
                    .setSingleChoiceItems(choices.map { it.first }.toTypedArray(), checked) { d, i ->
                        d.dismiss()
                        Threading.background {
                            run("timer") {
                                if (g != null) node.setGroupDisappearing(g, choices[i].second)
                                else node.setDisappearing(device, choices[i].second)
                            }
                            runOnUiThread {
                                Toast.makeText(this, "New messages: ${choices[i].first.lowercase()}", Toast.LENGTH_SHORT).show()
                            }
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    /** Calls this contact, or starts a call in this group: voice, or video with our camera on. */
    private fun call(video: Boolean = false) {
        if (!::node.isInitialized) return
        group?.let { g ->
            return Calls.startGroup(this, node, persona, g, convo?.title ?: "Group", video)
        }
        Calls.start(this, node, persona, device, convo?.title ?: Threnody.short(device), video)
    }

    private fun wifiDirect() {
        if (!WifiDirect.permitted(this)) return requestPermissions(arrayOf(WifiDirect.permission), WIFI_DIRECT)
        WifiDirect.host(applicationContext, node, device)
        Toast.makeText(this, "Setting up Wi-Fi Direct…", Toast.LENGTH_SHORT).show()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        Calls.permissionResult(code, results)
        if (code == WIFI_DIRECT && results.isNotEmpty() && results.all { it == 0 }) wifiDirect()
        if (code == VIDEO_PERMISSION && results.isNotEmpty() && results.all { it == 0 }) recordVideo()
        if (code == LOCATION_PERMISSION) {
            val then = Cards.pendingLocation
            Cards.pendingLocation = null
            if (results.any { it == 0 }) then?.invoke()
        }
        if (code == VOICE_PERMISSION && results.isNotEmpty() && results.all { it == 0 }) {
            Toast.makeText(this, "Hold the microphone to record", Toast.LENGTH_SHORT).show()
        }
    }

    /**
     * Runs a call on one scheduled message; false means it's being sent
     * right now (or already went), so nothing changed: say so.
     */
    private fun runScheduled(what: String, f: () -> Boolean) {
        var done = true
        run(what) { done = f() }
        if (!done) runOnUiThread { beingSent() }
    }

    private fun beingSent() {
        Toast.makeText(this, "It's being sent now", Toast.LENGTH_SHORT).show()
    }

    /** Runs a node call, reporting failure, then refreshes. */
    private fun run(what: String, f: () -> Unit) {
        try { f() } catch (e: Exception) {
            runOnUiThread { Toast.makeText(this, "Couldn't $what: ${e.message}", Toast.LENGTH_LONG).show() }
        }
        refresh()
    }

    // Android 10–12; from 13 the search box takes Back itself.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        if (search.isOpen) search.close() else super.onBackPressed()
    }

    override fun onDestroy() {
        fetchPoll.removeCallbacksAndMessages(null)
        // A draft never sent goes with the chat: its plain file is deleted.
        if (::draftBar.isInitialized) discardVoiceDraft()
        super.onDestroy()
    }

    companion object {
        private const val FETCH_POLL = 1
        /** Files larger than this (8 MiB) say they're being saved, as it takes a moment. */
        private const val SAVE_NOTICE = 8_388_608uL
        /** Longest quote snippet, as the node cuts it. */
        const val MAX_QUOTE_CHARS = 200
        /** Opens the notes to self. */
        const val NOTE = "note"
        const val KEY = "key"
        const val DEVICE = "device"
        const val GROUP = "group"
        /** With [GROUP]: the inviter (fingerprint) whose invitation to show. */
        const val INVITER = "inviter"
        const val PERSONA = "persona"
        /** Opened from a search result: the message to show (its time and sender device), and the query. */
        const val JUMP_AT_MS = "jump_at_ms"
        const val JUMP_DEVICE = "jump_device"
        const val JUMP_QUERY = "jump_query"
        /** Messages loaded at first, and the most loaded to reach an old match. */
        private const val HISTORY_PAGE = 200u
        private const val MAX_HISTORY = 25_600u
        /** Most matches a chat search lists. */
        private const val SEARCH_LIMIT = 1_000u
        /** Most photos or files sent at once. */
        private const val MAX_PICK = 30
        /** Messages from one sender this close together are drawn as one group. */
        private const val GROUP_GAP_MS = 3 * 60 * 1000L
        private const val WIFI_DIRECT = 2
        /** The *Quote* item in a message's text selection. */
        private const val QUOTE_ITEM = 0x51
        private const val LOCATION_PERMISSION = 7
        private const val VOICE_PERMISSION = 5
        private const val VIDEO_PERMISSION = 6
        /** We're taken to have stopped typing after this long without a keystroke. */
        private const val TYPING_IDLE_MS = 5_000L
        /** The contact's "…" goes after this long without word (refreshed while it types). */
        private const val PEER_TYPING_MS = 8_000L
        private const val TYPING_AGAIN_MS = 4_000L
    }
}
