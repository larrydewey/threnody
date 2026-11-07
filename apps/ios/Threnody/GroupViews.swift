import SwiftUI

/// A new group: its name and who to invite.
@MainActor
struct NewGroupView: View {
    let created: (ChatRef) -> Void
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var picked: Set<String> = []
    @State private var busy = false
    @State private var failure: String?

    /// Accepted contacts of the main identity.
    private var contacts: [Conversation] {
        store.chats.compactMap { $0.ref.persona == nil ? $0.conversation : nil }.filter { $0.accepted && !$0.blocked }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Group name", text: $name)
                }
                Section("Invite") {
                    if contacts.isEmpty { Text("Add contacts first.").foregroundStyle(.secondary) }
                    ForEach(contacts) { c in
                        Toggle(c.title, isOn: Binding(get: { picked.contains(c.device) }, set: { on in
                            if on { picked.insert(c.device) } else { picked.remove(c.device) }
                        }))
                    }
                }
            }
            .navigationTitle("New group")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create") { create() }
                        .disabled(busy || name.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .alert("Couldn't create the group", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(failure ?? "")
            }
        }
    }

    private func create() {
        guard let n = store.node else { return }
        let (title, invite) = (name.trimmingCharacters(in: .whitespaces), Array(picked))
        busy = true
        Task {
            do {
                let (g, failed) = try await offMain { () -> (String, [String]) in
                    let g = try n.createGroup(name: title)
                    var failed: [String] = []
                    for peer in invite {
                        do { try n.inviteToGroup(group: g, peer: peer) } catch { failed.append(error.localizedDescription) }
                    }
                    return (g, failed)
                }
                store.refresh()
                if !failed.isEmpty { failure = failed.joined(separator: "\n") }
                created(ChatRef(persona: nil, target: .group(g)))
            } catch {
                failure = error.localizedDescription
            }
            busy = false
        }
    }
}

/// A group's members and settings.
@MainActor
struct GroupInfoView: View {
    let ref: ChatRef
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var rows: [GroupMemberRow] = []
    @State private var level: NotifyLevel = .all
    @State private var timer: UInt32?
    @State private var asking: MemberAsk?
    @State private var inviting = false
    @State private var confirmLeave = false
    @State private var confirmDelete = false
    @State private var confirmClear = false
    @State private var failure: String?

    private var node: ThrenodyNode? { store.node(for: ref.persona) }
    private var id: String { ref.chatKey }
    private var group: GroupInfo? { store.row(ref)?.group }
    private var target: ChatTarget? { node.map { ChatTarget(node: $0, ref: ref, device: "") } }

    var body: some View {
        Form {
            if let g = group {
                Section {
                    LabeledContent("Name", value: g.name)
                    LabeledContent("Your role", value: Self.role(g.myRole))
                }
                Section {
                    ForEach(rows, id: \.fingerprint) { r in
                        Menu {
                            ForEach(r.actions.indices, id: \.self) { i in
                                Button(Self.label(r.actions[i])) { asking = MemberAsk(row: r, action: r.actions[i]) }
                            }
                        } label: {
                            HStack {
                                Text(r.isMe ? "You" : store.name(r.fingerprint, persona: ref.persona))
                                Spacer()
                                Text(Self.role(r.role) + (r.successor ? " · successor" : ""))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .disabled(r.actions.isEmpty)
                        .foregroundStyle(.primary)
                    }
                    if g.myRole != .member {
                        Button("Invite…", systemImage: "person.badge.plus") { inviting = true }
                    }
                } header: {
                    Text(g.members.count == 1 ? "1 member" : "\(g.members.count) members")
                } footer: {
                    Text("Tap a member for what you can do.")
                }
                Section("Notifications") {
                    Picker("Notify", selection: Binding(get: { level }, set: { l in
                        level = l
                        act { try $0.setGroupNotifyLevel(group: id, level: l) }
                    })) {
                        Text("All messages").tag(NotifyLevel.all)
                        Text("Mentions and replies").tag(NotifyLevel.mentions)
                        Text("Off").tag(NotifyLevel.off)
                    }
                }
                Section("Disappearing messages") {
                    Picker("Timer", selection: Binding(get: { timer }, set: { t in
                        timer = t
                        act { try $0.setGroupDisappearing(group: id, seconds: t) }
                    })) {
                        ForEach(Privacy.timers.indices, id: \.self) { i in
                            Text(Privacy.timers[i].0).tag(Privacy.timers[i].1)
                        }
                    }
                }
                Section {
                    Button("Clear chat on this device", role: .destructive) { confirmClear = true }
                    Button("Leave group", role: .destructive) { confirmLeave = true }
                    if g.owned { Button("Delete group for everyone", role: .destructive) { confirmDelete = true } }
                }
            } else {
                Text("You're no longer in this group.").foregroundStyle(.secondary)
            }
        }
        .navigationTitle(group?.name ?? "Group")
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .onReceive(store.events) { se in
            if se.persona == ref.persona && se.event.group == id { Task { await load() } }
        }
        .sheet(isPresented: $inviting) {
            InviteToGroupView(ref: ref, members: Set(rows.flatMap(\.devices) + rows.map(\.fingerprint)))
        }
        .confirmationDialog(asking.map { Self.label($0.action) + "?" } ?? "", isPresented: Binding(get: { asking != nil }, set: { if !$0 { asking = nil } }),
                            titleVisibility: .visible, presenting: asking) { a in
            Button(Self.label(a.action), role: a.action == .remove ? ButtonRole.destructive : nil) { perform(a) }
        } message: { a in
            Text(confirmText(a))
        }
        .confirmationDialog("Leave \(group?.name ?? "the group")?", isPresented: $confirmLeave, titleVisibility: .visible) {
            Button("Leave", role: .destructive) { leave() }
        } message: {
            Text("You stop receiving its messages. Its history stays on your devices; it can invite you again. If you're its only owner, make someone else owner first, or delete it.")
        }
        .confirmationDialog("Delete \(group?.name ?? "the group")?", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("Delete", role: .destructive) {
                act { try $0.deleteGroup(group: id) }
                dismiss()
            }
        } message: {
            Text("Everyone is removed and the group ends. Its history stays on your devices.")
        }
        .confirmationDialog("Clear this chat?", isPresented: $confirmClear, titleVisibility: .visible) {
            Button("Clear", role: .destructive) { act { try $0.clearGroupConversation(group: id) } }
        } message: {
            Text("Its messages are deleted from this device. Other members keep theirs.")
        }
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
    }

    static func role(_ r: MemberRole) -> String {
        switch r {
        case .owner: "Owner"
        case .admin: "Admin"
        case .member: "Member"
        }
    }

    static func label(_ a: GroupMemberAction) -> String {
        switch a {
        case .makeAdmin: "Make admin"
        case .removeAdmin: "Remove as admin"
        case .makeSuccessor: "Make successor"
        case .noLongerSuccessor: "No longer successor"
        case .makeOwner: "Make owner"
        case .useAsCoordinator: "Apply changes from this device"
        case .remove: "Remove from group"
        case .removeThisDevice: "Remove the lost coordinating device"
        case .stepDown: "Step down as admin"
        }
    }

    private func confirmText(_ a: MemberAsk) -> String {
        let name = a.row.isMe ? "You" : store.name(a.row.fingerprint, persona: ref.persona)
        let g = group?.name ?? "the group"
        switch a.action {
        case .makeAdmin: return "\(name) can invite and remove members and pin messages. Members will see which devices are yours and \(name)'s."
        case .removeAdmin: return "\(name) stays in \(g) as a member."
        case .makeSuccessor: return "If you're not seen in \(g) for 14 days, \(name) may take it over."
        case .noLongerSuccessor: return "Nobody will be able to take \(g) over if you disappear."
        case .makeOwner: return "\(name) owns \(g) from now on. You become an admin and can't undo this."
        case .useAsCoordinator: return "Changes to \(g) are applied from this device from now on."
        case .remove: return "They stop receiving new messages in \(g). They can be invited again later."
        case .removeThisDevice: return "This device takes over applying changes and removes your lost device from \(g)."
        case .stepDown: return "You stay in \(g) as a member."
        }
    }

    private func perform(_ a: MemberAsk) {
        let (g, peer, action) = (id, a.row.fingerprint, a.action)
        act { n in
            switch action {
            case .makeAdmin: try n.promoteInGroup(group: g, peer: peer)
            case .removeAdmin, .stepDown: try n.demoteInGroup(group: g, peer: peer)
            case .makeSuccessor: try n.setGroupSuccessor(group: g, peer: peer)
            case .noLongerSuccessor: try n.setGroupSuccessor(group: g, peer: nil)
            case .makeOwner: try n.transferGroupOwnership(group: g, peer: peer)
            case .useAsCoordinator: try n.useAsCoordinator(group: g, device: n.deviceFingerprint())
            case .remove: try n.removeFromGroup(group: g, peer: peer)
            case .removeThisDevice: try n.removeLostCoordinator(group: g)
            }
        }
    }

    private func leave() {
        guard let n = node else { return }
        let g = id
        Task {
            do {
                try await offMain { try n.leaveGroup(group: g) }
                store.refresh()
                dismiss()
            } catch {
                failure = error.localizedDescription
            }
        }
    }

    private func load() async {
        guard let n = node, let t = target else { return }
        let g = id
        rows = (try? await offMain { try n.groupMemberRows(group: g) }) ?? []
        level = (try? await offMain { t.notifyLevel() }) ?? .all
        timer = (try? await offMain { t.timer() }) ?? nil
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = node else { return }
        Task {
            do {
                try await offMain { try f(n) }
            } catch {
                failure = error.localizedDescription
            }
            store.refresh()
            await load()
        }
    }
}

/// A member and what to do to them, awaiting confirmation.
private struct MemberAsk {
    let row: GroupMemberRow
    let action: GroupMemberAction
}

/// Invites contacts who aren't in the group yet.
@MainActor
struct InviteToGroupView: View {
    let ref: ChatRef
    let members: Set<String>
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var picked: Set<String> = []
    @State private var failure: String?

    private var candidates: [Conversation] {
        store.chats.compactMap { $0.ref.persona == ref.persona ? $0.conversation : nil }
            .filter { c in c.accepted && !c.blocked && members.isDisjoint(with: c.devices + [c.key]) }
    }

    var body: some View {
        NavigationStack {
            Form {
                if candidates.isEmpty { Text("Everyone you could invite is in the group.").foregroundStyle(.secondary) }
                ForEach(candidates) { c in
                    Toggle(c.title, isOn: Binding(get: { picked.contains(c.device) }, set: { on in
                        if on { picked.insert(c.device) } else { picked.remove(c.device) }
                    }))
                }
            }
            .navigationTitle("Invite")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) { Button("Invite") { invite() }.disabled(picked.isEmpty) }
            }
            .alert("Couldn't invite", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) { dismiss() }
            } message: {
                Text(failure ?? "")
            }
        }
    }

    private func invite() {
        guard let n = store.node(for: ref.persona) else { return }
        let (g, peers) = (ref.chatKey, Array(picked))
        Task {
            let failed = (try? await offMain { () -> [String] in
                var failed: [String] = []
                for p in peers {
                    do { try n.inviteToGroup(group: g, peer: p) } catch { failed.append(error.localizedDescription) }
                }
                return failed
            }) ?? []
            store.refresh()
            if failed.isEmpty { dismiss() } else { failure = failed.joined(separator: "\n") }
        }
    }
}

/// An invitation to a group: join it, or decline.
@MainActor
struct GroupInviteView: View {
    let ref: ChatRef
    let group: String
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var failure: String?

    private var invite: GroupInvite? { store.row(ref)?.invite }

    var body: some View {
        VStack(spacing: 20) {
            Image(systemName: "person.3.fill").font(.system(size: 48)).foregroundStyle(.tint)
            if let i = invite {
                Text("\(store.name(i.from, persona: ref.persona)) invites you to \(i.name)")
                    .font(.title3).multilineTextAlignment(.center)
                Text("Members of a group see each other's messages and which devices are in it.")
                    .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                HStack(spacing: 16) {
                    Button("Decline", role: .destructive) { answer(accept: false) }
                        .buttonStyle(.bordered)
                    Button("Join") { answer(accept: true) }
                        .buttonStyle(.borderedProminent)
                }
            } else {
                Text("This invitation is no longer open.").foregroundStyle(.secondary)
            }
        }
        .padding(24)
        .navigationTitle("Invitation")
        .navigationBarTitleDisplayMode(.inline)
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
    }

    private func answer(accept: Bool) {
        guard let n = store.node(for: ref.persona), let i = invite else { return }
        let (g, from) = (group, i.from)
        Task {
            do {
                try await offMain {
                    if accept {
                        try n.acceptGroupInviteFrom(group: g, from: from)
                    } else {
                        try n.declineGroupInviteFrom(group: g, from: from)
                    }
                }
                store.refresh()
                if accept {
                    store.openChat = ChatRef(persona: ref.persona, target: .group(g))
                } else {
                    dismiss()
                }
            } catch {
                failure = error.localizedDescription
            }
        }
    }
}
