package org.threnody.app

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.net.Uri
import android.os.Build
import android.provider.CalendarContract
import android.provider.ContactsContract
import android.provider.Settings
import android.text.InputType
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ArrayAdapter
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import uniffi.threnody_ffi.CardInfo
import uniffi.threnody_ffi.CardItemInfo
import uniffi.threnody_ffi.SecretFieldInfo
import java.text.DateFormat
import java.util.Calendar
import java.util.Date

/**
 * Cards (Appendix V): a contact, a Wi-Fi network, a place, an event or a
 * secret sent as a message. Drawing one in a bubble, its actions, and
 * the forms that make one.
 */
object Cards {
    /** Field names a secret's fields may have, as people read them. */
    private val FIELD_LABELS = mapOf(
        "username" to "Username", "password" to "Password", "url" to "Website", "otp" to "One-time code",
        "number" to "Card number", "holder" to "Name on card", "expiry" to "Expires", "cvc" to "Security code",
        "pin" to "PIN", "text" to "Note",
    )

    fun fieldLabel(name: String) = FIELD_LABELS[name] ?: name.replaceFirstChar { it.uppercase() }

    /** What kind of card it is, as a heading (with its icon). */
    fun kindLabel(c: CardInfo): String = "${kindIcon(c)} ${kindName(c)}"

    fun kindIcon(c: CardInfo): String = when (val i = c.item) {
        is CardItemInfo.Contact -> "👤"
        is CardItemInfo.Wifi -> "📶"
        is CardItemInfo.Location -> "📍"
        is CardItemInfo.Event -> "📅"
        is CardItemInfo.Secret -> when (i.kind) { "login" -> "🔑"; "card" -> "💳"; else -> "🔒" }
    }

    fun kindName(c: CardInfo): String = when (val i = c.item) {
        is CardItemInfo.Contact -> "Contact"
        is CardItemInfo.Wifi -> "Wi-Fi network"
        is CardItemInfo.Location -> "Location"
        is CardItemInfo.Event -> "Event"
        is CardItemInfo.Secret -> when (i.kind) {
            "login" -> "Login"
            "card" -> "Payment card"
            "note" -> "Secure note"
            else -> "Secret"
        }
    }

    private fun value(i: CardItemInfo.Secret, name: String) = i.fields.firstOrNull { it.name == name }?.value ?: ""

    /** A payment card's fields drawn as the card, its number covered unless `revealed`. */
    fun paymentCard(a: Activity, i: CardItemInfo.Secret, revealed: Boolean): PaymentCardView =
        PaymentCardView(a).apply {
            val digits = value(i, "number")
            show(i.title, digits, PaymentCardView.shownNumber(digits, revealed), value(i, "holder"), value(i, "expiry"))
        }

    private fun degrees(e7: Int) = "%.6f".format(java.util.Locale.ROOT, e7 / 1e7)

    fun whenText(startMs: Long, endMs: Long, allDay: Boolean): String {
        val date = DateFormat.getDateInstance(DateFormat.MEDIUM)
        val time = DateFormat.getTimeInstance(DateFormat.SHORT)
        if (allDay) return date.format(Date(startMs)) + if (endMs > startMs) " – " + date.format(Date(endMs)) else ""
        val start = "${date.format(Date(startMs))}, ${time.format(Date(startMs))}"
        if (endMs <= startMs) return start
        val sameDay = date.format(Date(startMs)) == date.format(Date(endMs))
        return start + " – " + if (sameDay) time.format(Date(endMs)) else "${date.format(Date(endMs))}, ${time.format(Date(endMs))}"
    }

    /** A covered value: dots, but a card's number keeps its last four digits. */
    private fun covered(f: SecretFieldInfo): String = when (f.name) {
        "number" -> "•••• " + f.value.filter { it.isDigit() }.takeLast(4)
        "cvc", "pin" -> "•".repeat(f.value.length.coerceIn(3, 6))
        else -> "••••••••"
    }

    /**
     * The card in a bubble: a header with its kind and what it is, its
     * details, and buttons for what can be done with it. `open` shows it
     * in full (a secret's fields; a view-once card, which it opens for
     * good). A payment card is drawn as one.
     */
    fun view(a: Activity, c: CardInfo, outgoing: Boolean, fg: Int, connect: (String) -> Unit, open: () -> Unit): View {
        val tint = (fg and 0x00FFFFFF) or 0x1A000000
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            background = rounded(tint, a.dp(Design.radiusMd).toFloat())
            setPadding(a.dp(Design.md), a.dp(Design.md), a.dp(Design.md), a.dp(Design.sm))
            minimumWidth = a.dp(240)
        }
        val secret = c.item as? CardItemInfo.Secret
        val title = when (val i = c.item) {
            is CardItemInfo.Contact -> i.name
            is CardItemInfo.Wifi -> i.ssid
            is CardItemInfo.Location -> i.label.ifEmpty { "${degrees(i.latitudeE7)}, ${degrees(i.longitudeE7)}" }
            is CardItemInfo.Event -> i.title
            is CardItemInfo.Secret -> i.title
        }
        val sealed = c.viewOnce && !c.opened
        // Header: the kind's icon in a circle, its name, and what it is.
        box.addView(LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = android.view.Gravity.CENTER_VERTICAL
            addView(TextView(a).apply {
                text = if (sealed) "👁" else kindIcon(c)
                textSize = 18f
                gravity = android.view.Gravity.CENTER
                background = GradientDrawableCircle(tint)
            }, LinearLayout.LayoutParams(a.dp(40), a.dp(40)).apply { marginEnd = a.dp(Design.md) })
            addView(LinearLayout(a).apply {
                orientation = LinearLayout.VERTICAL
                addView(TextView(a).apply {
                    text = (if (sealed) "View once · " else "") + kindName(c)
                    textSize = 12f
                    setTextColor(fg)
                    alpha = 0.75f
                })
                val heading = when {
                    sealed -> if (outgoing) "Sent to view once" else "Tap Open to see it"
                    c.opened && secret == null -> if (outgoing) "Sent" else "Opened"
                    // The card itself shows its name; say which one it is.
                    secret?.kind == "card" -> value(secret, "number").filter { it.isDigit() }.takeLast(4)
                        .let { if (it.isEmpty()) title else "Ending $it" }
                    else -> title
                }
                addView(TextView(a).apply {
                    text = heading
                    textSize = 16f
                    setTextColor(fg)
                    setTypeface(typeface, Typeface.BOLD)
                    maxLines = 2
                    ellipsize = android.text.TextUtils.TruncateAt.END
                })
            }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        })
        val details = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, a.dp(Design.sm), 0, 0)
        }
        fun line(text: String, size: Float = 14f, dim: Boolean = false, mono: Boolean = false) {
            if (text.isBlank()) return
            details.addView(TextView(a).apply {
                this.text = text
                textSize = size
                setTextColor(fg)
                if (dim) alpha = 0.75f
                if (mono) typeface = Typeface.MONOSPACE
                setPadding(0, a.dp(1), 0, a.dp(1))
            })
        }
        val actions = LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, a.dp(Design.sm), 0, 0)
        }
        fun action(text: String, run: () -> Unit) {
            actions.addView(TextView(a).apply {
                this.text = text
                textSize = 14f
                setTextColor(fg)
                setTypeface(typeface, Typeface.BOLD)
                gravity = android.view.Gravity.CENTER
                minHeight = a.dp(36)
                background = android.graphics.drawable.RippleDrawable(
                    android.content.res.ColorStateList.valueOf(tint),
                    rounded(tint, a.dp(18).toFloat()), null,
                )
                setPadding(a.dp(Design.md), 0, a.dp(Design.md), 0)
                setOnClickListener { v -> Design.lightHaptic(v); run() }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { marginEnd = a.dp(Design.sm) })
        }
        when {
            sealed -> if (!outgoing) action("Open") { open() }
            c.opened -> line(if (outgoing) "What it held isn't kept on this device." else "It was viewed once and is gone.", 13f, dim = true)
            else -> when (val i = c.item) {
                is CardItemInfo.Contact -> {
                    i.phones.forEach { line("📞  $it") }
                    i.emails.forEach { line("✉️  $it") }
                    line(i.address, dim = true)
                    if (i.handle.isNotEmpty()) line(i.handle, dim = true)
                    line(i.note, dim = true)
                    val link = i.invite.ifEmpty { i.handle }
                    if (!outgoing && link.isNotEmpty()) action("Add on Threnody") { connect(link) }
                    action("Save") { saveContact(a, i) }
                }
                is CardItemInfo.Wifi -> {
                    line(listOfNotNull(
                        when (i.security) { "open" -> "Open network"; "wep" -> "WEP"; else -> "WPA/WPA2/WPA3" },
                        "hidden".takeIf { i.hidden },
                    ).joinToString(" · "), 13f, dim = true)
                    action("Join") { joinWifi(a, i) }
                    if (i.password.isNotEmpty()) action("Copy password") { copy(a, "Wi-Fi password", i.password, sensitive = true) }
                }
                is CardItemInfo.Location -> {
                    if (i.label.isNotEmpty()) line("${degrees(i.latitudeE7)}, ${degrees(i.longitudeE7)}", 13f, dim = true, mono = true)
                    if (i.accuracyM > 0u) line("Within ${i.accuracyM} m", 13f, dim = true)
                    action("Open in maps") { openMap(a, i) }
                }
                is CardItemInfo.Event -> {
                    line("🕒  " + whenText(i.startMs.toLong(), i.endMs.toLong(), i.allDay))
                    if (i.place.isNotEmpty()) line("📍  ${i.place}")
                    line(i.note, dim = true)
                    action("Add to calendar") { addToCalendar(a, i) }
                }
                is CardItemInfo.Secret -> {
                    if (i.kind == "card") {
                        details.addView(paymentCard(a, i, revealed = false), LinearLayout.LayoutParams(a.dp(260), WRAP_CONTENT))
                    } else {
                        for (f in i.fields.take(4)) {
                            line("${fieldLabel(f.name)}  ${if (f.hidden) covered(f) else f.value}", mono = f.hidden)
                        }
                    }
                    action("Show") { open() }
                }
            }
        }
        if (details.childCount > 0) box.addView(details)
        if (actions.childCount > 0) box.addView(actions)
        return box
    }

    /** A filled circle, for a card's icon. */
    @Suppress("FunctionName")
    private fun GradientDrawableCircle(color: Int) = android.graphics.drawable.GradientDrawable().apply {
        shape = android.graphics.drawable.GradientDrawable.OVAL
        setColor(color)
    }

    /**
     * A card in full, in a dialog: a secret's fields with *Reveal* and
     * *Copy* (copied as sensitive), or a view-once card's contents.
     */
    fun show(a: Activity, c: CardInfo, connect: (String) -> Unit) {
        val i = c.item
        if (i !is CardItemInfo.Secret) {
            // A view-once card of another kind: the same view, without
            // the Open button, in a dialog.
            val plain = c.copy(viewOnce = false)
            SecureBuilder(a)
                .setView(ScrollView(a).apply {
                    setPadding(a.dp(24), a.dp(16), a.dp(24), 0)
                    addView(view(a, plain, outgoing = false, fg = a.color(R.color.text), connect = connect) {})
                })
                .setPositiveButton("Done", null)
                .show()
            return
        }
        val box = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
        }
        if (i.kind == "card") {
            // The card itself; its Reveal shows the whole number on it.
            val card = paymentCard(a, i, revealed = false)
            box.addView(card, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = a.dp(Design.md) })
            val digits = value(i, "number")
            box.addView(LinearLayout(a).apply {
                orientation = LinearLayout.HORIZONTAL
                var shown = false
                addView(a.tertiaryButton("Reveal number") { b ->
                    shown = !shown
                    card.show(i.title, digits, PaymentCardView.shownNumber(digits, shown), value(i, "holder"), value(i, "expiry"))
                    (b as TextView).text = if (shown) "Hide number" else "Reveal number"
                })
                addView(a.tertiaryButton("Copy number") { copy(a, "Card number", digits.filter { it.isDigit() }, sensitive = true) })
            })
        }
        val onCard = if (i.kind == "card") setOf("number", "holder", "expiry") else emptySet()
        for (f in i.fields.filter { it.name !in onCard }) {
            box.addView(a.label(fieldLabel(f.name), 12f, R.color.muted))
            val value = TextView(a).apply {
                text = if (f.hidden) covered(f) else f.value
                textSize = 17f
                setTextColor(a.color(R.color.text))
                if (f.name in setOf("number", "password", "cvc", "pin", "otp")) typeface = Typeface.MONOSPACE
            }
            box.addView(value)
            val row = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL }
            if (f.hidden) row.addView(a.tertiaryButton("Reveal") { b ->
                val shown = value.text == f.value
                value.text = if (shown) covered(f) else f.value
                (b as TextView).text = if (shown) "Reveal" else "Hide"
            })
            row.addView(a.tertiaryButton("Copy") { copy(a, fieldLabel(f.name), f.value, sensitive = f.hidden || f.name != "url") })
            box.addView(row)
        }
        if (c.viewOnce) box.addView(a.label("Viewed once: it's gone from this chat when you close it.", 13f, R.color.muted, maxLines = 0))
        SecureBuilder(a)
            .setTitle(i.title)
            .setView(ScrollView(a).apply { addView(box) })
            .setPositiveButton("Done", null)
            .show()
    }

    /** Copies `value`; marked sensitive, so Android 13+ hides it from its preview and keyboards' clipboard history. */
    fun copy(a: Activity, label: String, value: String, sensitive: Boolean) {
        val clip = ClipData.newPlainText(label, value)
        if (sensitive) {
            clip.description.extras = android.os.PersistableBundle().apply {
                putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true)
            }
        }
        a.getSystemService(ClipboardManager::class.java)?.setPrimaryClip(clip)
        if (Build.VERSION.SDK_INT < 33) Toast.makeText(a, "Copied", Toast.LENGTH_SHORT).show()
    }

    private fun start(a: Activity, intent: Intent, none: String) {
        try {
            AppLock.expectReturn()
            a.startActivity(intent)
        } catch (_: Exception) {
            Toast.makeText(a, none, Toast.LENGTH_LONG).show()
        }
    }

    private fun saveContact(a: Activity, c: CardItemInfo.Contact) {
        val intent = Intent(Intent.ACTION_INSERT, ContactsContract.Contacts.CONTENT_URI).apply {
            putExtra(ContactsContract.Intents.Insert.NAME, c.name)
            c.phones.getOrNull(0)?.let { putExtra(ContactsContract.Intents.Insert.PHONE, it) }
            c.phones.getOrNull(1)?.let { putExtra(ContactsContract.Intents.Insert.SECONDARY_PHONE, it) }
            c.phones.getOrNull(2)?.let { putExtra(ContactsContract.Intents.Insert.TERTIARY_PHONE, it) }
            c.emails.getOrNull(0)?.let { putExtra(ContactsContract.Intents.Insert.EMAIL, it) }
            c.emails.getOrNull(1)?.let { putExtra(ContactsContract.Intents.Insert.SECONDARY_EMAIL, it) }
            c.emails.getOrNull(2)?.let { putExtra(ContactsContract.Intents.Insert.TERTIARY_EMAIL, it) }
            if (c.address.isNotEmpty()) putExtra(ContactsContract.Intents.Insert.POSTAL, c.address)
            if (c.note.isNotEmpty()) putExtra(ContactsContract.Intents.Insert.NOTES, c.note)
        }
        start(a, intent, "No contacts app to save it in.")
    }

    private fun joinWifi(a: Activity, w: CardItemInfo.Wifi) {
        if (Build.VERSION.SDK_INT >= 30 && w.security != "wep") {
            val s = android.net.wifi.WifiNetworkSuggestion.Builder().setSsid(w.ssid).apply {
                if (w.security == "wpa") setWpa2Passphrase(w.password)
                if (w.hidden) setIsHiddenSsid(true)
            }.build()
            val intent = Intent(Settings.ACTION_WIFI_ADD_NETWORKS)
                .putParcelableArrayListExtra(Settings.EXTRA_WIFI_NETWORK_LIST, arrayListOf(s))
            start(a, intent, "Couldn't open Wi-Fi settings.")
        } else {
            // Older Android, or WEP (which it no longer adds): the password
            // on the clipboard, and the Wi-Fi settings to choose it in.
            if (w.password.isNotEmpty()) copy(a, "Wi-Fi password", w.password, sensitive = true)
            start(a, Intent(Settings.ACTION_WIFI_SETTINGS), "Couldn't open Wi-Fi settings.")
        }
    }

    private fun openMap(a: Activity, l: CardItemInfo.Location) {
        val lat = degrees(l.latitudeE7)
        val lon = degrees(l.longitudeE7)
        val q = Uri.encode("$lat,$lon" + if (l.label.isNotEmpty()) "(${l.label})" else "")
        start(a, Intent(Intent.ACTION_VIEW, Uri.parse("geo:$lat,$lon?q=$q")), "No maps app to open it in.")
    }

    private fun addToCalendar(a: Activity, e: CardItemInfo.Event) {
        val intent = Intent(Intent.ACTION_INSERT, CalendarContract.Events.CONTENT_URI).apply {
            putExtra(CalendarContract.Events.TITLE, e.title)
            putExtra(CalendarContract.EXTRA_EVENT_BEGIN_TIME, e.startMs.toLong())
            if (e.endMs > 0uL) putExtra(CalendarContract.EXTRA_EVENT_END_TIME, e.endMs.toLong())
            putExtra(CalendarContract.EXTRA_EVENT_ALL_DAY, e.allDay)
            if (e.place.isNotEmpty()) putExtra(CalendarContract.Events.EVENT_LOCATION, e.place)
            if (e.note.isNotEmpty()) putExtra(CalendarContract.Events.DESCRIPTION, e.note)
        }
        start(a, intent, "No calendar app to add it to.")
    }

    // ---- Making cards ----

    private fun form(a: Activity) = LinearLayout(a).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(a.dp(20), a.dp(8), a.dp(20), 0)
    }

    private fun field(a: Activity, hint: String, type: Int = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES, value: String = "") =
        EditText(a).apply {
            this.hint = hint
            inputType = type
            setText(value)
        }

    private fun ask(a: Activity, title: String, body: View, make: () -> CardInfo?, send: (CardInfo) -> Unit) {
        SecureBuilder(a)
            .setTitle(title)
            .setView(ScrollView(a).apply { addView(body) })
            .setPositiveButton("Send") { _, _ ->
                val c = make()
                if (c == null) Toast.makeText(a, "Fill in what it needs first.", Toast.LENGTH_LONG).show() else send(c)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun card(item: CardItemInfo, viewOnce: Boolean = false, expiresInS: UInt = 0u) =
        CardInfo(item, viewOnce, false, false, expiresInS)

    /** A contact: ourselves (`invite` our link, `handle` ours if any), or someone typed in or picked. */
    fun composeContact(a: Activity, name: String, phone: String, email: String, invite: String, handle: String, send: (CardInfo) -> Unit) {
        val body = form(a)
        val n = field(a, "Name", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS, name)
        val p = field(a, "Phone", InputType.TYPE_CLASS_PHONE, phone)
        val e = field(a, "Email", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS, email)
        val addr = field(a, "Address").wrapping(max = 4)
        val note = field(a, "Note").wrapping(max = 4)
        listOf(n, p, e, addr, note).forEach { body.addView(it) }
        if (invite.isNotEmpty() || handle.isNotEmpty()) {
            body.addView(a.label("With your Threnody ${if (handle.isNotEmpty()) "handle $handle" else "invite link"}, so they can add you.",
                13f, R.color.muted, maxLines = 0))
        }
        ask(a, "Share a contact", body, {
            val item = CardItemInfo.Contact(
                n.text.toString().trim(), invite, handle,
                listOf(p.text.toString().trim()).filter { it.isNotEmpty() },
                listOf(e.text.toString().trim()).filter { it.isNotEmpty() },
                addr.text.toString().trim(), note.text.toString().trim(),
            )
            card(item).takeIf { item.name.isNotEmpty() && (invite.isNotEmpty() || handle.isNotEmpty() || item.phones.isNotEmpty() || item.emails.isNotEmpty() || item.address.isNotEmpty()) }
        }, send)
    }

    fun composeWifi(a: Activity, send: (CardInfo) -> Unit) {
        val body = form(a)
        val ssid = field(a, "Network name", InputType.TYPE_CLASS_TEXT)
        val pw = field(a, "Password", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD)
        val security = Spinner(a).apply {
            adapter = ArrayAdapter(a, android.R.layout.simple_spinner_dropdown_item, listOf("WPA/WPA2/WPA3", "Open", "WEP"))
        }
        val hidden = CheckBox(a).apply { text = "Hidden network" }
        listOf(ssid, pw, security, hidden).forEach { body.addView(it) }
        ask(a, "Share a Wi-Fi network", body, {
            val sec = when (security.selectedItemPosition) { 1 -> "open"; 2 -> "wep"; else -> "wpa" }
            val name = ssid.text.toString().trim()
            card(CardItemInfo.Wifi(name, if (sec == "open") "" else pw.text.toString(), sec, hidden.isChecked))
                .takeIf { name.isNotEmpty() }
        }, send)
    }

    fun composeEvent(a: Activity, send: (CardInfo) -> Unit) {
        val body = form(a)
        val title = field(a, "What")
        val place = field(a, "Where")
        val note = field(a, "Note").wrapping(max = 4)
        val start = Calendar.getInstance().apply {
            add(Calendar.HOUR_OF_DAY, 1)
            set(Calendar.MINUTE, 0)
            set(Calendar.SECOND, 0)
            set(Calendar.MILLISECOND, 0)
        }
        var hours = 1
        val allDay = CheckBox(a).apply { text = "All day" }
        val whenView = TextView(a).apply {
            textSize = 16f
            setTextColor(a.color(R.color.accent))
            setPadding(0, a.dp(12), 0, a.dp(12))
        }
        fun refresh() {
            val end = start.timeInMillis + hours * 3_600_000L
            whenView.text = whenText(start.timeInMillis, if (allDay.isChecked) 0 else end, allDay.isChecked)
        }
        whenView.setOnClickListener {
            android.app.DatePickerDialog(a, { _, y, m, d ->
                start.set(y, m, d)
                if (allDay.isChecked) refresh() else android.app.TimePickerDialog(a, { _, h, min ->
                    start.set(Calendar.HOUR_OF_DAY, h)
                    start.set(Calendar.MINUTE, min)
                    refresh()
                }, start.get(Calendar.HOUR_OF_DAY), start.get(Calendar.MINUTE), android.text.format.DateFormat.is24HourFormat(a)).show()
            }, start.get(Calendar.YEAR), start.get(Calendar.MONTH), start.get(Calendar.DAY_OF_MONTH)).show()
        }
        allDay.setOnCheckedChangeListener { _, _ -> refresh() }
        refresh()
        body.addView(title)
        body.addView(whenView)
        body.addView(allDay)
        body.addView(place)
        body.addView(note)
        ask(a, "Share an event", body, {
            val t = title.text.toString().trim()
            val s = if (allDay.isChecked) {
                // An all-day event is its date, at midnight UTC.
                Calendar.getInstance(java.util.TimeZone.getTimeZone("UTC")).apply {
                    clear()
                    set(start.get(Calendar.YEAR), start.get(Calendar.MONTH), start.get(Calendar.DAY_OF_MONTH))
                }.timeInMillis
            } else start.timeInMillis
            val end = if (allDay.isChecked) 0L else start.timeInMillis + hours * 3_600_000L
            card(CardItemInfo.Event(t, s.toULong(), end.toULong(), allDay.isChecked,
                place.text.toString().trim(), note.text.toString().trim())).takeIf { t.isNotEmpty() }
        }, send)
    }

    /** A place: where the phone is now (asking for location first), labelled if the user wants. */
    fun composeLocation(a: Activity, requestCode: Int, send: (CardInfo) -> Unit) {
        val need = arrayOf(Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION)
        if (need.none { a.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }) {
            pendingLocation = { composeLocation(a, requestCode, send) }
            a.requestPermissions(need, requestCode)
            return
        }
        val lm = a.getSystemService(android.location.LocationManager::class.java) ?: return
        val provider = listOf(android.location.LocationManager.FUSED_PROVIDER, android.location.LocationManager.GPS_PROVIDER,
            android.location.LocationManager.NETWORK_PROVIDER).firstOrNull { lm.isProviderEnabled(it) }
        if (provider == null) {
            Toast.makeText(a, "Location is off. Turn it on in Settings to share where you are.", Toast.LENGTH_LONG).show()
            return
        }
        Toast.makeText(a, "Finding where you are…", Toast.LENGTH_SHORT).show()
        val got: (android.location.Location?) -> Unit = { loc ->
            if (loc == null) {
                Toast.makeText(a, "Couldn't find where you are.", Toast.LENGTH_LONG).show()
            } else {
                val body = form(a)
                val label = field(a, "Label (optional)")
                body.addView(a.label("%.5f, %.5f · within %d m".format(java.util.Locale.ROOT, loc.latitude, loc.longitude, loc.accuracy.toInt()),
                    14f, R.color.muted))
                body.addView(label)
                ask(a, "Share this location", body, {
                    card(CardItemInfo.Location((loc.latitude * 1e7).toInt(), (loc.longitude * 1e7).toInt(),
                        label.text.toString().trim(), loc.accuracy.toUInt()))
                }, send)
            }
        }
        try {
            if (Build.VERSION.SDK_INT >= 30) {
                lm.getCurrentLocation(provider, null, a.mainExecutor) { got(it) }
            } else {
                @Suppress("DEPRECATION")
                lm.requestSingleUpdate(provider, { got(it) }, a.mainLooper)
            }
        } catch (e: SecurityException) {
            got(null)
        }
    }

    /** Whether the secret being made goes to our own notes. */
    private var notes = false

    /** Set while location permission is asked for. */
    var pendingLocation: (() -> Unit)? = null

    /** A login, a payment card or a secure note, typed in. Seen once and gone after a day unless the user says otherwise. */
    fun composeSecret(a: Activity, inNotes: Boolean = false, send: (CardInfo) -> Unit) {
        notes = inNotes
        val kinds = listOf("Login" to "login", "Payment card" to "card", "Secure note" to "note")
        SecureBuilder(a)
            .setTitle("Share a secret")
            .setItems(kinds.map { it.first }.toTypedArray()) { _, i -> secretForm(a, kinds[i].second, send) }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun secretForm(a: Activity, kind: String, send: (CardInfo) -> Unit) {
        if (kind == "card") return paymentCardForm(a, send)
        val body = form(a)
        val title = field(a, when (kind) { "login" -> "Name (e.g. the site)"; "card" -> "Name (e.g. Joint Visa)"; else -> "Title" })
        body.addView(title)
        val pw = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        val digits = InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
        val fields: List<Triple<String, EditText, Boolean>> = when (kind) {
            "login" -> listOf(
                Triple("username", field(a, "Username", InputType.TYPE_CLASS_TEXT), false),
                Triple("password", field(a, "Password", pw), true),
                Triple("url", field(a, "Website", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI), false),
            )
            "card" -> listOf(
                Triple("number", field(a, "Card number", digits), true),
                Triple("holder", field(a, "Name on card", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS), false),
                Triple("expiry", field(a, "Expires (MM/YY)", InputType.TYPE_CLASS_DATETIME), false),
                Triple("cvc", field(a, "Security code", digits), true),
            )
            else -> listOf(Triple("text", field(a, "Note", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS).wrapping(max = 8), true))
        }
        fields.forEach {
            // Keyboards are asked not to learn from what goes in here.
            it.second.imeOptions = it.second.imeOptions or android.view.inputmethod.EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
            body.addView(it.second)
        }
        val (once, expiry) = secretOptions(a, body)
        ask(a, "Share a secret", body, {
            val t = title.text.toString().trim()
            val fs = fields.map { (name, f, hidden) -> SecretFieldInfo(name, f.text.toString().trim(), hidden) }.filter { it.value.isNotEmpty() }
            val secs = when (expiry.selectedItemPosition) { 0 -> 3600u; 2 -> 7u * 86400u; else -> 86400u }
            card(CardItemInfo.Secret(kind, t, fs, ""), viewOnce = once.isChecked, expiresInS = secs)
                .takeIf { t.isNotEmpty() && fs.isNotEmpty() }
        }, send)
    }

    /** How long a secret stays, and whether it's seen once: the end of every secret form. */
    private fun secretOptions(a: Activity, body: LinearLayout): Pair<CheckBox, Spinner> {
        // In our own notes there's nobody to view it once.
        val once = CheckBox(a).apply { text = "They can view it once"; isChecked = !notes; visibility = if (notes) View.GONE else View.VISIBLE }
        val expiry = Spinner(a).apply {
            adapter = ArrayAdapter(a, android.R.layout.simple_spinner_dropdown_item, listOf("Gone after 1 hour", "Gone after 1 day", "Gone after 1 week"))
            setSelection(1)
        }
        body.addView(once)
        body.addView(expiry)
        body.addView(a.label("It isn't kept on this phone once sent, and isn't backed up.", 13f, R.color.muted, maxLines = 0))
        return once to expiry
    }

    private fun expirySeconds(s: Spinner): UInt = when (s.selectedItemPosition) { 0 -> 3600u; 2 -> 7u * 86400u; else -> 86400u }

    /**
     * A payment card, typed onto a picture of it: the card above updates
     * as each field is filled, the number falls into its groups, the
     * expiry into MM/YY, and a mistyped number is pointed out.
     */
    private fun paymentCardForm(a: Activity, send: (CardInfo) -> Unit) {
        val body = form(a)
        val preview = PaymentCardView(a)
        body.addView(preview, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply {
            topMargin = a.dp(Design.sm)
            bottomMargin = a.dp(Design.md)
        })
        val secret = InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
        fun box(hint: String, type: Int, max: Int = 0) = com.google.android.material.textfield.TextInputLayout(a).apply {
            this.hint = hint
            boxBackgroundMode = com.google.android.material.textfield.TextInputLayout.BOX_BACKGROUND_OUTLINE
            setBoxCornerRadii(a.dp(10).toFloat(), a.dp(10).toFloat(), a.dp(10).toFloat(), a.dp(10).toFloat())
            addView(com.google.android.material.textfield.TextInputEditText(context).apply {
                inputType = type
                imeOptions = android.view.inputmethod.EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
                if (max > 0) filters = arrayOf(android.text.InputFilter.LengthFilter(max))
            })
        }
        val title = box("Name it (Joint Visa, say)", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_WORDS, 40)
        val number = box("Card number", InputType.TYPE_CLASS_NUMBER, 23)
        val holder = box("Name on card", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS, 40)
        val expiry = box("MM/YY", InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_FLAG_SIGNED, 5)
        val cvc = box("CVC", secret, 4)
        fun text(t: com.google.android.material.textfield.TextInputLayout) = t.editText?.text?.toString() ?: ""
        val row = LinearLayout(a).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(expiry, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginEnd = a.dp(Design.sm) })
            addView(cvc, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        }
        for (v in listOf(title, number, holder)) body.addView(v, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = a.dp(Design.xs) })
        body.addView(row)
        // A number pad that also takes the spaces and slash the fields are shaped with.
        number.editText?.keyListener = android.text.method.DigitsKeyListener.getInstance("0123456789 ")
        expiry.editText?.keyListener = android.text.method.DigitsKeyListener.getInstance("0123456789/")
        cvc.endIconMode = com.google.android.material.textfield.TextInputLayout.END_ICON_PASSWORD_TOGGLE
        fun redraw() {
            val digits = text(number).filter { it.isDigit() }
            preview.show(text(title), digits, PaymentCardView.shownNumber(digits, revealed = true), text(holder), text(expiry))
            number.error = if (digits.length >= 12 && !uniffi.threnody_ffi.cardNumberValid(digits) &&
                digits.length >= (if (uniffi.threnody_ffi.cardBrand(digits) == "amex") 15 else 16)) "Check the number: a digit looks wrong" else null
        }
        // Formats as typed; deleting is left alone, so a space or slash can go.
        fun format(t: com.google.android.material.textfield.TextInputLayout, f: (String) -> String) {
            val e = t.editText ?: return
            var busy = false
            var before = 0
            e.addTextChangedListener(object : android.text.TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, st: Int, c: Int, after: Int) { before = s?.length ?: 0 }
                override fun onTextChanged(s: CharSequence?, st: Int, b: Int, c: Int) {}
                override fun afterTextChanged(s: android.text.Editable?) {
                    if (busy || s == null) return
                    if (s.length > before) {
                        val shaped = f(s.toString())
                        if (shaped != s.toString()) {
                            busy = true
                            s.replace(0, s.length, shaped)
                            busy = false
                        }
                    }
                    redraw()
                }
            })
        }
        format(number) { uniffi.threnody_ffi.formatCardNumber(it) }
        format(expiry) { uniffi.threnody_ffi.formatCardExpiry(it) }
        format(title) { it }
        format(holder) { it }
        format(cvc) { it }
        redraw()
        val (once, expires) = secretOptions(a, body)
        ask(a, "Share a payment card", body, {
            val digits = text(number).filter { it.isDigit() }
            val fields = listOf(
                SecretFieldInfo("number", uniffi.threnody_ffi.formatCardNumber(digits), true),
                SecretFieldInfo("holder", text(holder).trim(), false),
                SecretFieldInfo("expiry", uniffi.threnody_ffi.formatCardExpiry(text(expiry).trim()).trimEnd('/'), false),
                SecretFieldInfo("cvc", text(cvc).trim(), true),
            ).filter { it.value.isNotEmpty() }
            val t = text(title).trim().ifEmpty { PaymentCardView.BRANDS[uniffi.threnody_ffi.cardBrand(digits)]?.let { "$it card" } ?: "" }
            card(CardItemInfo.Secret("card", t, fields, ""), viewOnce = once.isChecked, expiresInS = expirySeconds(expires))
                .takeIf { t.isNotEmpty() && digits.length >= 12 }
        }, send)
    }

    @Suppress("unused")
    private val keepImports = listOf(MATCH_PARENT, WRAP_CONTENT)
}
