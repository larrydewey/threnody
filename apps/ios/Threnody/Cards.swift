import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Cards (Appendix V): a contact, a Wi-Fi network, a place, an event or a
/// secret sent as a message. Drawing one in a bubble, showing one in full,
/// and the forms that make one.
enum Cards {
    static func fieldLabel(_ name: String) -> String {
        switch name {
        case "username": return "Username"
        case "password": return "Password"
        case "url": return "Website"
        case "otp": return "One-time code"
        case "number": return "Card number"
        case "holder": return "Name on card"
        case "expiry": return "Expires"
        case "cvc": return "Security code"
        case "pin": return "PIN"
        case "text": return "Note"
        default: return name.prefix(1).uppercased() + name.dropFirst()
        }
    }

    static func kindLabel(_ c: CardInfo) -> String {
        switch c.item {
        case .contact: return "👤 Contact"
        case .wifi: return "📶 Wi-Fi network"
        case .location: return "📍 Location"
        case .event: return "📅 Event"
        case let .secret(kind, _, _, _):
            switch kind {
            case "login": return "🔑 Login"
            case "card": return "💳 Payment card"
            case "note": return "🔒 Secure note"
            default: return "🔒 Secret"
            }
        }
    }

    static func degrees(_ e7: Int32) -> String { String(format: "%.6f", Double(e7) / 1e7) }

    /// A covered value: dots, but a card's number keeps its last four digits.
    static func covered(_ f: SecretFieldInfo) -> String {
        f.name == "number" ? "•••• " + String(f.value.filter(\.isNumber).suffix(4)) : "••••••••"
    }

    static func whenText(start: UInt64, end: UInt64, allDay: Bool) -> String {
        let s = Date(timeIntervalSince1970: Double(start) / 1000)
        let e = Date(timeIntervalSince1970: Double(end) / 1000)
        if allDay {
            var f = Date.FormatStyle(date: .abbreviated, time: .omitted)
            f.timeZone = TimeZone(identifier: "UTC")!
            return end > start ? "\(s.formatted(f)) – \(e.formatted(f))" : s.formatted(f)
        }
        let full = s.formatted(date: .abbreviated, time: .shortened)
        guard end > start else { return full }
        let sameDay = Calendar.current.isDate(s, inSameDayAs: e)
        return full + " – " + (sameDay ? e.formatted(date: .omitted, time: .shortened)
                                       : e.formatted(date: .abbreviated, time: .shortened))
    }

    /// Copies `value`, kept on this device only, and gone in a minute if
    /// it is secret.
    static func copy(_ value: String, secret: Bool) {
        var options: [UIPasteboard.OptionsKey: Any] = [.localOnly: true]
        if secret { options[.expirationDate] = Date().addingTimeInterval(60) }
        UIPasteboard.general.setItems([[UTType.plainText.identifier: value]], options: options)
    }

    private static func escape(_ s: String) -> String {
        s.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: ";", with: "\\;")
            .replacingOccurrences(of: ",", with: "\\,").replacingOccurrences(of: "\n", with: "\\n")
    }

    /// A file to hand to Contacts or Calendar through the share sheet.
    static func file(named name: String, _ text: String) -> URL? {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("cards", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appendingPathComponent(name)
        return (try? text.write(to: url, atomically: true, encoding: .utf8)) != nil ? url : nil
    }

    static func vcard(name: String, phones: [String], emails: [String], address: String, note: String) -> String {
        var v = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:\(escape(name))\r\nN:\(escape(name));;;;\r\n"
        for p in phones { v += "TEL:\(escape(p))\r\n" }
        for e in emails { v += "EMAIL:\(escape(e))\r\n" }
        if !address.isEmpty { v += "ADR:;;\(escape(address));;;;\r\n" }
        if !note.isEmpty { v += "NOTE:\(escape(note))\r\n" }
        return v + "END:VCARD\r\n"
    }

    static func ics(title: String, start: UInt64, end: UInt64, allDay: Bool, place: String, note: String) -> String {
        let utc = DateFormatter()
        utc.timeZone = TimeZone(identifier: "UTC")
        utc.locale = Locale(identifier: "en_US_POSIX")
        func at(_ ms: UInt64, _ fmt: String) -> String {
            utc.dateFormat = fmt
            return utc.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
        }
        let span: String
        if allDay {
            let last = (end > start ? end : start) + 86_400_000
            span = "DTSTART;VALUE=DATE:\(at(start, "yyyyMMdd"))\r\nDTEND;VALUE=DATE:\(at(last, "yyyyMMdd"))"
        } else {
            let stop = end > start ? end : start + 3_600_000
            span = "DTSTART:\(at(start, "yyyyMMdd'T'HHmmss'Z'"))\r\nDTEND:\(at(stop, "yyyyMMdd'T'HHmmss'Z'"))"
        }
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        var v = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Threnody//Cards//EN\r\nBEGIN:VEVENT\r\n"
        v += "UID:\(start)-\(UInt32.random(in: 0...UInt32.max))@threnody\r\nDTSTAMP:\(at(now, "yyyyMMdd'T'HHmmss'Z'"))\r\n"
        v += span + "\r\nSUMMARY:\(escape(title))\r\n"
        if !place.isEmpty { v += "LOCATION:\(escape(place))\r\n" }
        if !note.isEmpty { v += "DESCRIPTION:\(escape(note))\r\n" }
        return v + "END:VEVENT\r\nEND:VCALENDAR\r\n"
    }

    /// Degrees × 10⁷ from "lat, lon", a `geo:` URI, or a map link with
    /// `ll=`, `q=`, `mlat=`/`mlon=` or `@lat,lon` in it.
    static func parsePlace(_ raw: String) -> (Int32, Int32)? {
        let s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        func pair(_ t: Substring) -> (Double, Double)? {
            let parts = t.split(separator: ",", maxSplits: 2)
            guard parts.count >= 2,
                  let lat = Double(parts[0].trimmingCharacters(in: .whitespaces)),
                  let lon = Double(parts[1].split(whereSeparator: { "?;&/z(".contains($0) }).first?
                      .trimmingCharacters(in: .whitespaces) ?? "")
            else { return nil }
            return (lat, lon)
        }
        func after(_ key: String) -> Substring? {
            guard let r = s.range(of: key) else { return nil }
            return s[r.upperBound...].split(whereSeparator: { "&#".contains($0) }).first
        }
        var found: (Double, Double)?
        if s.hasPrefix("geo:") {
            found = pair(s.dropFirst(4))
        } else if let a = after("mlat="), let b = after("mlon="), let lat = Double(a), let lon = Double(b) {
            found = (lat, lon)
        } else if let v = after("ll=") ?? after("q=") {
            found = pair(Substring(v.removingPercentEncoding ?? String(v)))
        } else if let r = s.range(of: "@") {
            found = pair(s[r.upperBound...])
        } else {
            found = pair(Substring(s))
        }
        guard let (lat, lon) = found, (-90...90).contains(lat), (-180...180).contains(lon) else { return nil }
        return (Int32((lat * 1e7).rounded()), Int32((lon * 1e7).rounded()))
    }

    static func card(_ item: CardItemInfo, viewOnce: Bool = false, expiresInS: UInt32 = 0) -> CardInfo {
        CardInfo(item: item, viewOnce: viewOnce, noForward: false, opened: false, expiresInS: expiresInS)
    }
}

/// A payment card drawn as one: the ID-1 shape, its network's colours, a
/// chip, the number in groups, the holder and the expiry.
struct PaymentCardView: View {
    let title: String
    let digits: String
    /// What the number line says (covered but for the last four, or all).
    let shown: String
    let holder: String
    let expiry: String

    static let brands = ["visa": "VISA", "mastercard": "mastercard", "amex": "AMEX", "discover": "DISCOVER",
                         "diners": "Diners Club", "jcb": "JCB", "unionpay": "UnionPay"]
    private static let colours: [String: (UInt32, UInt32)] = [
        "visa": (0x1A1F71, 0x3949C8), "mastercard": (0x1F1F1F, 0xB4410E), "amex": (0x0B4F7C, 0x2E9BD6),
        "discover": (0x2B1A10, 0xD9731C), "diners": (0x0F3D5E, 0x5B8DB8), "jcb": (0x0E4D2E, 0x2F9E5E),
        "unionpay": (0x7A0E1C, 0x1C5D8C),
    ]

    static func shownNumber(_ digits: String, revealed: Bool) -> String {
        let d = digits.filter(\.isNumber)
        if revealed || d.isEmpty { return formatCardNumber(number: d) }
        return "•••• •••• •••• " + String(d.suffix(4))
    }

    private static func colour(_ hex: UInt32) -> Color {
        Color(red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
              blue: Double(hex & 0xFF) / 255)
    }

    var body: some View {
        let brand = cardBrand(number: digits)
        let (from, to) = Self.colours[brand] ?? (0x334155, 0x64748B)
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(title).font(.subheadline.bold()).lineLimit(1)
                Spacer()
                Text(Self.brands[brand] ?? "").font(.headline.weight(.heavy).italic())
            }
            Spacer(minLength: 6)
            RoundedRectangle(cornerRadius: 5)
                .fill(LinearGradient(colors: [Self.colour(0xE9D38C), Self.colour(0xB8963E)],
                                     startPoint: .topLeading, endPoint: .bottomTrailing))
                .frame(width: 36, height: 27)
                .padding(.bottom, 8)
            Text(shown.isEmpty ? "•••• •••• •••• ••••" : shown)
                .font(.system(.title3, design: .monospaced))
                .minimumScaleFactor(0.6)
                .lineLimit(1)
                .shadow(color: .black.opacity(0.3), radius: 1, y: 1)
            Spacer(minLength: 6)
            HStack(alignment: .bottom) {
                VStack(alignment: .leading, spacing: 1) {
                    Text("CARDHOLDER").font(.system(size: 8)).tracking(1).opacity(0.75)
                    Text(holder.isEmpty ? "—" : holder.uppercased()).font(.footnote.bold()).lineLimit(1)
                }
                Spacer()
                VStack(alignment: .trailing, spacing: 1) {
                    Text("EXPIRES").font(.system(size: 8)).tracking(1).opacity(0.75)
                    Text(expiry.isEmpty ? "MM/YY" : expiry).font(.footnote.bold())
                }
            }
        }
        .foregroundStyle(.white)
        .padding(16)
        .aspectRatio(1.586, contentMode: .fit)
        .background(LinearGradient(colors: [Self.colour(from), Self.colour(to)],
                                   startPoint: .topLeading, endPoint: .bottomTrailing),
                    in: RoundedRectangle(cornerRadius: 16))
        .shadow(color: .black.opacity(0.2), radius: 3, y: 2)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel([Self.brands[brand] ?? "Payment card", title, "ending " + String(digits.filter(\.isNumber).suffix(4))]
            .filter { !$0.isEmpty }.joined(separator: ", "))
    }
}

extension Cards {
    static func value(_ fields: [SecretFieldInfo], _ name: String) -> String {
        fields.first { $0.name == name }?.value ?? ""
    }
}

/// A card in a bubble: its kind, what it is, and what can be done with it.
struct CardContent: View {
    let card: CardInfo
    let out: Bool
    /// Shows it in full (a secret, or a view-once card, which it opens).
    let onOpen: () -> Void
    /// Adds the person a contact card names (by link or handle); nil where
    /// adding isn't offered.
    let onAdd: ((String) -> Void)?
    @Environment(\.openURL) private var openURL

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(Cards.kindLabel(card)).font(.caption).opacity(0.75)
            if card.viewOnce && !card.opened {
                Text(out ? "Sent to view once" : "View once").bold()
                if !out { Button("Open", action: onOpen).buttonStyle(.bordered) }
            } else if card.opened {
                if case let .secret(_, title, _, _) = card.item { Text(title).bold() }
                Text(out ? "Its contents aren't kept on this device." : "Opened").font(.caption).opacity(0.75)
            } else {
                details
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder private var details: some View {
        switch card.item {
        case let .contact(name, invite, handle, phones, emails, address, note):
            Text(name).bold()
            ForEach(phones + emails, id: \.self) { Text($0).textSelection(.enabled) }
            if !address.isEmpty { Text(address).font(.callout).opacity(0.8) }
            if !handle.isEmpty { Text(handle).font(.callout).opacity(0.8) }
            if !note.isEmpty { Text(note).font(.callout).opacity(0.8) }
            HStack {
                let link = invite.isEmpty ? handle : invite
                if !out, !link.isEmpty, let onAdd { Button("Add") { onAdd(link) } }
                if let url = Cards.file(named: "contact.vcf", Cards.vcard(name: name, phones: phones, emails: emails,
                                                                         address: address, note: note)) {
                    ShareLink("Save", item: url)
                }
            }
            .buttonStyle(.bordered)
        case let .wifi(ssid, password, security, hidden):
            Text(ssid).bold()
            Text([security == "open" ? "Open" : security == "wep" ? "WEP" : "WPA", hidden ? "hidden" : nil]
                .compactMap { $0 }.joined(separator: " · ")).font(.caption).opacity(0.75)
            HStack {
                Button("Copy name") { Cards.copy(ssid, secret: false) }
                if !password.isEmpty { Button("Copy password") { Cards.copy(password, secret: true) } }
            }
            .buttonStyle(.bordered)
        case let .location(lat, lon, label, accuracy):
            let coords = "\(Cards.degrees(lat)), \(Cards.degrees(lon))"
            Text(label.isEmpty ? coords : label).bold()
            if !label.isEmpty { Text(coords).font(.caption).opacity(0.75) }
            if accuracy > 0 { Text("within \(accuracy) m").font(.caption).opacity(0.75) }
            Button("Open in Maps") {
                var c = URLComponents(string: "maps://")!
                c.queryItems = [URLQueryItem(name: "ll", value: "\(Cards.degrees(lat)),\(Cards.degrees(lon))"),
                                URLQueryItem(name: "q", value: label.isEmpty ? coords : label)]
                if let u = c.url { openURL(u) }
            }
            .buttonStyle(.bordered)
        case let .event(title, start, end, allDay, place, note):
            Text(title).bold()
            Text(Cards.whenText(start: start, end: end, allDay: allDay)).font(.callout)
            if !place.isEmpty { Text(place).font(.callout).opacity(0.8) }
            if !note.isEmpty { Text(note).font(.callout).opacity(0.8) }
            if let url = Cards.file(named: "event.ics", Cards.ics(title: title, start: start, end: end, allDay: allDay,
                                                                 place: place, note: note)) {
                ShareLink("Add to Calendar", item: url).buttonStyle(.bordered)
            }
        case let .secret(kind, title, fields, _):
            if kind == "card" {
                let digits = Cards.value(fields, "number")
                PaymentCardView(title: title, digits: digits, shown: PaymentCardView.shownNumber(digits, revealed: false),
                                holder: Cards.value(fields, "holder"), expiry: Cards.value(fields, "expiry"))
                    .frame(width: 260)
            } else {
                Text(title).bold()
                ForEach(Array(fields.prefix(4).enumerated()), id: \.offset) { _, f in
                    Text("\(Cards.fieldLabel(f.name)): \(f.hidden ? Cards.covered(f) : f.value)")
                        .font(.callout).opacity(f.hidden ? 0.75 : 1)
                }
            }
            Button("Show", action: onOpen).buttonStyle(.bordered)
        }
    }
}

/// A card shown in full: a secret's fields with Reveal and Copy, or a
/// view-once card's contents.
struct CardDetailSheet: View {
    let card: CardInfo
    let onAdd: ((String) -> Void)?
    @Environment(\.dismiss) private var dismiss
    @State private var shown: Set<Int> = []
    @State private var numberShown = false

    var body: some View {
        NavigationStack {
            Form {
                if case let .secret(kind, title, fields, _) = card.item {
                    if kind == "card" {
                        let digits = Cards.value(fields, "number")
                        Section {
                            PaymentCardView(title: title, digits: digits,
                                            shown: PaymentCardView.shownNumber(digits, revealed: numberShown),
                                            holder: Cards.value(fields, "holder"), expiry: Cards.value(fields, "expiry"))
                                .listRowInsets(EdgeInsets())
                                .listRowBackground(Color.clear)
                            Button(numberShown ? "Hide Number" : "Reveal Number") { numberShown.toggle() }
                            Button("Copy Number") { Cards.copy(digits.filter(\.isNumber), secret: true) }
                        }
                    }
                    ForEach(Array(fields.enumerated()).filter { kind != "card" || !["number", "holder", "expiry"].contains($0.element.name) },
                            id: \.offset) { i, f in
                        Section(Cards.fieldLabel(f.name)) {
                            Text(f.hidden && !shown.contains(i) ? Cards.covered(f) : f.value)
                                .font(["number", "password", "cvc", "pin", "otp"].contains(f.name)
                                      ? .body.monospaced() : .body)
                            if f.hidden {
                                Button(shown.contains(i) ? "Hide" : "Reveal") {
                                    if shown.contains(i) { shown.remove(i) } else { shown.insert(i) }
                                }
                            }
                            Button("Copy") { Cards.copy(f.value, secret: f.hidden || f.name != "url") }
                        }
                    }
                    if card.viewOnce {
                        Text("Viewed once: it's gone from this chat when you close this.").font(.footnote)
                    }
                } else {
                    // A view-once card's contents, shown as any card.
                    CardContent(card: CardInfo(item: card.item, viewOnce: false, noForward: card.noForward,
                                               opened: card.opened, expiresInS: card.expiresInS),
                                out: false, onOpen: {}, onAdd: onAdd)
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }

    private var title: String {
        if case let .secret(_, title, _, _) = card.item { return title }
        return Cards.kindLabel(card)
    }
}

/// What the attach menu can make.
enum CardKind: String, Identifiable {
    case contact, myContact, wifi, location, event, login, paymentCard, note
    var id: String { rawValue }
}

/// The form for one kind of card; `send` gets it.
struct CardComposer: View {
    let kind: CardKind
    /// For our own contact: this identity's invite link and handle.
    let invite: String
    let handle: String
    let send: (CardInfo) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var a = ""
    @State private var b = ""
    @State private var c = ""
    @State private var d = ""
    @State private var e = ""
    @State private var security = "wpa"
    @State private var hidden = false
    @State private var start = Calendar.current.date(byAdding: .hour, value: 1, to: Date()) ?? Date()
    @State private var hours = 1
    @State private var allDay = false
    @State private var viewOnce = true
    @State private var expiry: UInt32 = 86_400

    var body: some View {
        NavigationStack {
            Form { fields }
                .navigationTitle(title)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Send") {
                            if let card = make() {
                                dismiss()
                                send(card)
                            }
                        }
                        .disabled(make() == nil)
                    }
                }
        }
    }

    private var title: String {
        switch kind {
        case .contact, .myContact: return "Share a Contact"
        case .wifi: return "Share a Wi-Fi Network"
        case .location: return "Share a Location"
        case .event: return "Share an Event"
        case .login: return "Share a Login"
        case .paymentCard: return "Share a Payment Card"
        case .note: return "Share a Secure Note"
        }
    }

    @ViewBuilder private var fields: some View {
        switch kind {
        case .contact, .myContact:
            TextField("Name", text: $a).textContentType(.name)
            TextField("Phone", text: $b).keyboardType(.phonePad)
            TextField("Email", text: $c).keyboardType(.emailAddress).textInputAutocapitalization(.never)
            TextField("Address", text: $d, axis: .vertical)
            TextField("Note", text: $e, axis: .vertical)
            if kind == .myContact {
                Text(handle.isEmpty ? "With your Threnody invite link, so they can add you."
                                    : "With your Threnody handle \(handle), so they can add you.").font(.footnote)
            }
        case .wifi:
            TextField("Network name", text: $a).textInputAutocapitalization(.never).autocorrectionDisabled()
            Picker("Security", selection: $security) {
                Text("WPA/WPA2/WPA3").tag("wpa")
                Text("Open").tag("open")
                Text("WEP").tag("wep")
            }
            if security != "open" { SecureField("Password", text: $b) }
            Toggle("Hidden network", isOn: $hidden)
        case .location:
            TextField("Coordinates or a map link", text: $a, axis: .vertical)
                .textInputAutocapitalization(.never).autocorrectionDisabled()
            TextField("Label (optional)", text: $b)
            Text("Paste coordinates (51.5007, -0.1246) or a link from Maps (Share → Copy).").font(.footnote)
        case .event:
            TextField("What", text: $a)
            Toggle("All day", isOn: $allDay)
            DatePicker("Starts", selection: $start, displayedComponents: allDay ? [.date] : [.date, .hourAndMinute])
            if !allDay { Stepper("Lasts \(hours) h", value: $hours, in: 1...12) }
            TextField("Where", text: $b)
            TextField("Note", text: $c, axis: .vertical)
        case .login, .paymentCard, .note:
            TextField(kind == .login ? "Name (the site, say)" : kind == .paymentCard ? "Name it (Joint Visa, say)" : "Title",
                      text: $a)
            switch kind {
            case .login:
                TextField("Username", text: $b).textInputAutocapitalization(.never).autocorrectionDisabled()
                SecureField("Password", text: $c)
                TextField("Website", text: $d).keyboardType(.URL).textInputAutocapitalization(.never)
            case .paymentCard:
                let digits = b.filter(\.isNumber)
                PaymentCardView(title: a, digits: digits, shown: PaymentCardView.shownNumber(digits, revealed: true),
                                holder: c, expiry: d)
                    .listRowInsets(EdgeInsets(top: 8, leading: 0, bottom: 8, trailing: 0))
                    .listRowBackground(Color.clear)
                TextField("Card number", text: $b)
                    .keyboardType(.numberPad)
                    .font(.body.monospaced())
                    .onChange(of: b) { old, new in
                        // Shaped as typed; deleting is left alone.
                        if new.count > old.count { let f = formatCardNumber(number: new); if f != new { b = f } }
                    }
                if digits.count >= (cardBrand(number: digits) == "amex" ? 15 : 16) && !cardNumberValid(number: digits) {
                    Text("Check the number: a digit looks wrong").font(.footnote).foregroundStyle(.red)
                }
                TextField("Name on card", text: $c).textInputAutocapitalization(.characters)
                HStack {
                    TextField("MM/YY", text: $d)
                        .keyboardType(.numberPad)
                        .onChange(of: d) { old, new in
                            if new.count > old.count { let f = formatCardExpiry(typed: new); if f != new { d = f } }
                        }
                    Divider()
                    SecureField("CVC", text: $e).keyboardType(.numberPad)
                }
            default:
                TextField("Note", text: $b, axis: .vertical).autocorrectionDisabled()
            }
            Toggle("They can view it once", isOn: $viewOnce)
            Picker("Gone after", selection: $expiry) {
                Text("1 hour").tag(UInt32(3600))
                Text("1 day").tag(UInt32(86_400))
                Text("1 week").tag(UInt32(7 * 86_400))
            }
            Text("It isn't kept on this iPhone once sent, and isn't backed up.").font(.footnote)
        }
    }

    private func trimmed(_ s: String) -> String { s.trimmingCharacters(in: .whitespacesAndNewlines) }

    private func make() -> CardInfo? {
        switch kind {
        case .contact, .myContact:
            let name = trimmed(a)
            let phones = [trimmed(b)].filter { !$0.isEmpty }
            let emails = [trimmed(c)].filter { !$0.isEmpty }
            let mine = kind == .myContact
            let item = CardItemInfo.contact(name: name, invite: mine ? invite : "", handle: mine ? handle : "",
                                            phones: phones, emails: emails, address: trimmed(d), note: trimmed(e))
            let reachable = (mine && (!invite.isEmpty || !handle.isEmpty)) || !phones.isEmpty || !emails.isEmpty
                || !trimmed(d).isEmpty
            return name.isEmpty || !reachable ? nil : Cards.card(item)
        case .wifi:
            let ssid = trimmed(a)
            return ssid.isEmpty ? nil : Cards.card(.wifi(ssid: ssid, password: security == "open" ? "" : b,
                                                         security: security, hidden: hidden))
        case .location:
            guard let (lat, lon) = Cards.parsePlace(a) else { return nil }
            return Cards.card(.location(latitudeE7: lat, longitudeE7: lon, label: trimmed(b), accuracyM: 0))
        case .event:
            let t = trimmed(a)
            guard !t.isEmpty else { return nil }
            var s = UInt64(start.timeIntervalSince1970 * 1000)
            var end = s + UInt64(hours) * 3_600_000
            if allDay {
                // An all-day event is its date, at midnight UTC.
                let parts = Calendar.current.dateComponents([.year, .month, .day], from: start)
                var utc = Calendar(identifier: .gregorian)
                utc.timeZone = TimeZone(identifier: "UTC")!
                s = UInt64((utc.date(from: parts) ?? start).timeIntervalSince1970 * 1000)
                end = 0
            }
            return Cards.card(.event(title: t, startMs: s, endMs: end, allDay: allDay, place: trimmed(b),
                                     note: trimmed(c)))
        case .login, .paymentCard, .note:
            let t = trimmed(a)
            let raw: [(String, String, Bool)]
            switch kind {
            case .login: raw = [("username", b, false), ("password", c, true), ("url", d, false)]
            case .paymentCard:
                raw = [("number", formatCardNumber(number: b), true), ("holder", c, false),
                       ("expiry", String(formatCardExpiry(typed: d).trimmingCharacters(in: CharacterSet(charactersIn: "/"))), false),
                       ("cvc", e, true)]
            default: raw = [("text", b, true)]
            }
            let fields = raw.map { SecretFieldInfo(name: $0.0, value: trimmed($0.1), hidden: $0.2) }
                .filter { !$0.value.isEmpty }
            var title = t
            if kind == .paymentCard {
                guard b.filter(\.isNumber).count >= 12 else { return nil }
                if title.isEmpty { title = (PaymentCardView.brands[cardBrand(number: b)] ?? "Payment") + " card" }
            }
            guard !title.isEmpty, !fields.isEmpty else { return nil }
            let k = kind == .login ? "login" : kind == .paymentCard ? "card" : "note"
            return Cards.card(.secret(kind: k, title: title, fields: fields, source: ""), viewOnce: viewOnce,
                              expiresInS: expiry)
        }
    }
}
