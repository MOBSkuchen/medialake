import AppKit
import SwiftUI

// MARK: - Page

struct SyncPage: View {
    @ObservedObject var model: SyncModel
    @State private var sheet: SyncSheet?
    @State private var confirmLeave: SyncFolder?

    enum SyncSheet: Identifiable {
        case join, share, invite
        case browse(SyncFolder)
        var id: String {
            switch self {
            case .join: return "join"
            case .share: return "share"
            case .invite: return "invite"
            case .browse(let f): return "browse-" + f.id
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            PageTitle(text: "Sync")
            VStack(alignment: .leading, spacing: 6) {
                Toggle("Keep folders in sync with other computers", isOn: Binding(get: { model.enabled }, set: { v in Task { await model.setEnabled(v) } }))
                Hint("Files of a synced folder appear under ~/MediaLake and are loaded when you open them. Changes you make are sent to the computer that shares the folder, and from there to everyone else.")
            }
            if !model.message.isEmpty {
                Text(model.message).foregroundColor(.red).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true)
            }
            if model.enabled {
                foldersSection
                if !model.conflicts.isEmpty { conflictsSection }
                hostingSection
                speedSection
            }
        }
        .task { await model.refresh() }
        .sheet(item: $sheet) { s in
            switch s {
            case .join: JoinSheet(model: model) { sheet = nil }
            case .share: ShareSheet(model: model) { sheet = nil }
            case .invite: InviteSheet(model: model) { sheet = nil }
            case .browse(let f): BrowseSheet(model: model, folder: f) { sheet = nil }
            }
        }
        .confirmationDialog("Leave \(confirmLeave?.name ?? "")?", isPresented: Binding(get: { confirmLeave != nil }, set: { if !$0 { confirmLeave = nil } })) {
            Button("Leave and keep the files I have downloaded") {
                if let f = confirmLeave { Task { await model.leave(f, keepFiles: true) } }
            }
            Button("Leave and delete the files on this Mac", role: .destructive) {
                if let f = confirmLeave { Task { await model.leave(f, keepFiles: false) } }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The folder is unmounted. The files stay on the computer that shares them.")
        }
    }

    // MARK: my folders

    private var foldersSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("My folders").fontWeight(.semibold)
            if model.folders.isEmpty {
                Text("No synced folders yet").opacity(0.6)
            }
            ForEach(model.folders) { f in
                FolderCard(model: model, folder: f, limitGb: model.cacheLimitGb[f.id] ?? 10,
                           onBrowse: { sheet = .browse(f) }, onLeave: { confirmLeave = f })
            }
            HStack(spacing: 12) {
                Button("Add a folder…") { sheet = .join }
                if model.folders.count > 1 {
                    Button(model.folders.allSatisfy { $0.paused } ? "Resume all" : "Pause all") {
                        let pause = !model.folders.allSatisfy { $0.paused }
                        Task { await model.setAllPaused(pause) }
                    }
                }
            }
        }
    }

    // MARK: conflicts

    private var conflictsSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Conflicts").fontWeight(.semibold)
            Hint("These files were changed here and on the other computer. Pick what to keep.")
            VStack(spacing: 0) {
                ForEach(model.conflicts) { c in
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(c.path).lineLimit(1).truncationMode(.middle)
                            Text(c.hostDeleted ? "\(c.name): deleted on the other computer, edited here" : "\(c.name): edited in both places")
                                .font(.system(size: 12)).opacity(0.6)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        Button("Keep mine") { Task { await model.resolve(c, "mine") } }
                        Button("Use theirs") { Task { await model.resolve(c, "server") } }
                        Button("Keep both") { Task { await model.resolve(c, "both") } }
                    }
                    .padding(.vertical, 6)
                }
            }
        }
    }

    // MARK: hosting

    private var hostingSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Sharing from this Mac").fontWeight(.semibold)
            if !model.hostingError.isEmpty {
                Text(model.hostingError).foregroundColor(.red).font(.system(size: 12))
            } else if model.hostingRunning {
                Hint("Other computers reach this Mac at \(model.hostingAddress).")
            }
            if model.hosted.isEmpty {
                Text("Nothing is shared yet").opacity(0.6)
            }
            ForEach(model.hosted) { h in
                VStack(alignment: .leading, spacing: 4) {
                    HStack {
                        Text(h.name).fontWeight(.semibold)
                        Spacer()
                        Button("Snapshot now") { Task { await model.snapshot(h) } }.font(.system(size: 12))
                        Button("Stop sharing") { Task { await model.stopSharing(h) } }.font(.system(size: 12))
                    }
                    Text(h.root).font(.system(size: 12)).opacity(0.7).lineLimit(1).truncationMode(.middle)
                    Text("\(h.files.formatted()) files · \(formatBytes(h.bytes)) · backups: \(h.backup)").font(.system(size: 12)).opacity(0.7)
                    ForEach(h.issues, id: \.self) { Text($0).font(.system(size: 12)).foregroundColor(.orange) }
                }
                .padding(10)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
            }
            HStack(spacing: 12) {
                Button("Share a folder…") { sheet = .share }
                Button("Invite a device…") { sheet = .invite }.disabled(model.hosted.isEmpty)
            }
            if !model.clients.isEmpty {
                Text("Devices").fontWeight(.semibold).padding(.top, 6)
                ForEach(model.clients) { c in
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Text(c.name)
                            if c.embed { Text("search").font(.system(size: 11)).padding(.horizontal, 6).background(Capsule().fill(Color.accentColor.opacity(0.2))) }
                            Spacer()
                            Button("Remove device") { Task { await model.removeDevice(c) } }.font(.system(size: 12))
                        }
                        ForEach(model.hosted) { h in
                            HStack {
                                Text(h.name).font(.system(size: 12)).opacity(0.8)
                                Spacer()
                                Picker("", selection: Binding(
                                    get: { c.syncs[h.name] ?? "none" },
                                    set: { v in Task { await model.setAccess(c, sync: h.name, access: v) } })) {
                                    Text("No access").tag("none")
                                    Text("Read only").tag("ro")
                                    Text("Read and write").tag("rw")
                                }
                                .labelsHidden().frame(width: 160)
                            }
                        }
                    }
                    .padding(10)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
                }
            }
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "info.circle")
                Text("A laptop is a poor host: other computers cannot exchange changes while it sleeps. For always-on sharing, run medialake-server on a computer that stays on.")
                    .fixedSize(horizontal: false, vertical: true)
            }
            .font(.system(size: 12)).padding(10)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.accentColor.opacity(0.12)))
        }
    }

    // MARK: speed

    private var speedSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("This device").fontWeight(.semibold)
            HStack {
                Text("Name shown to others").frame(width: 190, alignment: .leading)
                TextField(model.systemName, text: $model.deviceName).textFieldStyle(.roundedBorder).frame(width: 220)
            }
            Hint("Other computers list this device under this name, and it is part of the names of conflict copies. Empty means this Mac's own name.")
                .font(.system(size: 12))
            LimitRow(title: "Limit upload speed", kbps: $model.uploadLimit)
            LimitRow(title: "Limit download speed", kbps: $model.downloadLimit)
            Button("Save") { Task { await model.saveSyncSettings() } }
        }
    }
}

/// A speed limit with an on/off switch: off means unlimited (stored as 0), on shows the number.
private struct LimitRow: View {
    let title: String
    @Binding var kbps: Int
    /// What the switch brings back when it is turned on again.
    @State private var remembered = 8000

    var body: some View {
        HStack(spacing: 10) {
            Toggle(title, isOn: Binding(
                get: { kbps > 0 },
                set: { on in
                    if on {
                        kbps = max(remembered, 1)
                    } else {
                        if kbps > 0 { remembered = kbps }
                        kbps = 0
                    }
                }))
                .frame(width: 190, alignment: .leading)
            if kbps > 0 {
                TextField("", value: $kbps, format: .number.grouping(.never)).textFieldStyle(.roundedBorder).frame(width: 90)
                Text("kbit/s").font(.system(size: 12)).opacity(0.7)
            }
        }
    }
}

// MARK: - One synced folder

private struct FolderCard: View {
    @ObservedObject var model: SyncModel
    let folder: SyncFolder
    let limitGb: Int
    let onBrowse: () -> Void
    let onLeave: () -> Void
    @State private var limit = 10

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Image(systemName: folder.symbol).foregroundColor(folder.symbolColor)
                Text(folder.name).fontWeight(.semibold)
                Text(folder.stateText).opacity(0.75)
                Spacer()
                if folder.readOnly { Text("Read only").font(.system(size: 11)).padding(.horizontal, 6).background(Capsule().fill(Color.primary.opacity(0.1))) }
            }
            Text(folder.path).font(.system(size: 12)).opacity(0.7).lineLimit(1).truncationMode(.middle)
            if let t = folder.lastSync {
                Text("From \(folder.hostName) · last synced \(t.formatted(.relative(presentation: .named)))").font(.system(size: 12)).opacity(0.6)
            } else if !folder.hostName.isEmpty {
                Text("From \(folder.hostName)").font(.system(size: 12)).opacity(0.6)
            }
            if folder.isMount {
                Text("\(formatBytes(folder.cachedBytes)) kept on this Mac" + (folder.cacheLimitBytes > 0 ? " (up to \(formatBytes(folder.cacheLimitBytes)))" : ""))
                    .font(.system(size: 12)).opacity(0.6)
            }
            if folder.readOnly {
                Text("This device may only read this folder: files open as usual, changing and deleting them is refused.")
                    .font(.system(size: 12)).opacity(0.6).fixedSize(horizontal: false, vertical: true)
            }
            if !folder.notice.isEmpty {
                HStack(alignment: .top, spacing: 6) {
                    Image(systemName: "info.circle")
                    Text(folder.notice).fixedSize(horizontal: false, vertical: true)
                }
                .font(.system(size: 12)).foregroundColor(.orange)
            }
            if folder.state == "held", let n = folder.heldDeletes {
                HStack {
                    Text("\(n) files would be deleted on the other computer.").font(.system(size: 12))
                    Button("Allow") { Task { await model.confirmDeletes(folder) } }.font(.system(size: 12))
                }
            }
            HStack(spacing: 10) {
                Button("Open in Finder") { ShellActions.open(folder.path) }
                if folder.isMount { Button("Keep on this Mac…", action: onBrowse) } else { Button("Browse…", action: onBrowse) }
                Button(folder.paused ? "Resume" : "Pause") { Task { await model.setPaused(folder, !folder.paused) } }
                Button("Check again") { Task { await model.verify(folder) } }.disabled(folder.paused)
                Button("Leave…", action: onLeave)
            }
            .font(.system(size: 12))
            HStack(spacing: 6) {
                Text("When both sides changed a file").font(.system(size: 12)).fixedSize()
                Picker("", selection: Binding(get: { folder.conflict }, set: { v in Task { await model.set(folder, ["conflict": v]) } })) {
                    Text("the other computer wins, mine is kept as a copy").tag("server_wins")
                    Text("ask me").tag("ask")
                }
                .labelsHidden().frame(width: 320)
            }
            if folder.isMount {
                HStack(spacing: 6) {
                    Text("Keep up to").font(.system(size: 12)).fixedSize()
                    Stepper(value: $limit, in: 1...2000, step: 5) { Text("\(limit) GB").font(.system(size: 12)).frame(width: 60, alignment: .trailing) }
                        .onChange(of: limit) { _, v in if v != limitGb { Task { await model.set(folder, ["cache_limit_gb": v]) } } }
                    Text("downloaded on this Mac").font(.system(size: 12)).opacity(0.7)
                }
            }
        }
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
        .onAppear { limit = limitGb }
        .onChange(of: limitGb) { _, v in limit = v }
    }
}

// MARK: - Join

struct JoinSheet: View {
    @ObservedObject var model: SyncModel
    let close: () -> Void

    @State private var code = ""
    @State private var hostId = ""
    @State private var available: [AvailableSyncRow] = []
    @State private var chosen = ""
    @State private var location = ""
    @State private var conflict = "server_wins"
    @State private var fullCopy = false
    @State private var error = ""
    @State private var working = false

    private var selected: AvailableSyncRow? { available.first { $0.id == chosen } }
    /// The computers to pick from: one that has removed this Mac is not offered (its folders, if any are
    /// still joined, say what happened).
    private var offered: [HostRow] { model.hosts.filter { $0.state != "revoked" } }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add a synced folder").font(.title2).fontWeight(.semibold)
            if !offered.isEmpty {
                HStack {
                    Text("Computer").frame(width: 90, alignment: .leading)
                    Picker("", selection: $hostId) {
                        Text("Choose…").tag("")
                        ForEach(offered) { h in Text(h.label).tag(h.id) }
                    }
                    .labelsHidden().frame(width: 320)
                    .onChange(of: hostId) { _, h in
                        if h.isEmpty {
                            available = []
                            chosen = ""
                        } else {
                            Task { await load(h) }
                        }
                    }
                    // a computer that is no longer used (it stopped sharing, it was replaced) can go
                    if let h = offered.first(where: { $0.id == hostId }), h.folders == 0 {
                        Button("Forget") {
                            let id = hostId
                            hostId = ""
                            Task { await model.forgetHost(id) }
                        }
                    }
                }
                Text("or paste an invitation code").opacity(0.6).font(.system(size: 12))
            }
            HStack {
                TextField("SN1-…  (from \"Invite a device\" on the other computer)", text: $code).textFieldStyle(.roundedBorder)
                Button("Connect") { Task { await connect() } }.disabled(code.trimmingCharacters(in: .whitespaces).isEmpty || working)
            }
            if !available.isEmpty {
                Divider()
                Text("Folder").fontWeight(.semibold)
                ForEach(available) { a in
                    HStack {
                        Image(systemName: chosen == a.id ? "largecircle.fill.circle" : "circle")
                        Text(a.name)
                        Text("\(a.files.formatted()) files · \(formatBytes(a.bytes))" + (a.access == "ro" ? " · read only" : ""))
                            .font(.system(size: 12)).opacity(0.6)
                        Spacer()
                        if a.joined { Text("already added").font(.system(size: 12)).opacity(0.6) }
                    }
                    .contentShape(Rectangle())
                    .onTapGesture {
                        if !a.joined {
                            chosen = a.id
                            location = SyncModel.defaultLocation(for: a.name)
                        }
                    }
                    .opacity(a.joined ? 0.5 : 1)
                }
                if selected != nil {
                    HStack {
                        Text("Location").frame(width: 90, alignment: .leading)
                        TextField("", text: $location).textFieldStyle(.roundedBorder)
                        Button("Choose…") { chooseLocation() }
                    }
                    HStack {
                        Text("Conflicts").frame(width: 90, alignment: .leading)
                        Picker("", selection: $conflict) {
                            Text("The other computer wins, mine is kept as a copy").tag("server_wins")
                            Text("Ask me").tag("ask")
                        }
                        .labelsHidden().frame(width: 320)
                    }
                    Toggle("Keep a full copy in a normal folder", isOn: $fullCopy)
                    Hint(fullCopy
                        ? "Every file is downloaded and stored in the folder. Use this if the folder must also work without MediaLake running."
                        : "Files are listed at once and downloaded when you open them. Pin what you need offline.")
                }
            }
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true) }
            HStack {
                Spacer()
                Button("Cancel") { close() }
                Button("Add") { Task { await join() } }
                    .buttonStyle(.borderedProminent)
                    .disabled(selected == nil || location.isEmpty || working)
            }
        }
        .padding(20).frame(width: 560)
        // what is known shows at once; then each computer is asked, and one that has removed this Mac
        // meanwhile drops out
        .task { await model.checkHosts() }
    }

    private func connect() async {
        working = true
        defer { working = false }
        error = ""
        if let id = await model.pair(code: code) {
            hostId = id
            code = ""
            await load(id)
        } else {
            error = model.message
        }
    }

    private func load(_ h: String) async {
        working = true
        defer { working = false }
        available = await model.available(host: h)
        if !model.message.isEmpty { error = model.message }
        chosen = available.first(where: { !$0.joined })?.id ?? ""
        if let a = selected { location = SyncModel.defaultLocation(for: a.name) }
        // a computer that turned this Mac away is off the list now: nothing stays selected
        if !offered.contains(where: { $0.id == h }) { hostId = "" }
    }

    private func chooseLocation() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true
        p.canChooseFiles = false
        p.canCreateDirectories = true
        p.prompt = "Choose"
        if p.runModal() == .OK, let u = p.url, let a = selected {
            location = u.appendingPathComponent(a.name).path
        }
    }

    private func join() async {
        working = true
        defer { working = false }
        let ok = await model.join(host: hostId, syncId: chosen, path: location, conflict: conflict, fullCopy: fullCopy)
        if ok { close() } else { error = model.message }
    }
}

// MARK: - Share, invite

struct ShareSheet: View {
    @ObservedObject var model: SyncModel
    let close: () -> Void
    @State private var root = ""
    @State private var name = ""
    @State private var backup = "file:10"
    @State private var error = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Share a folder").font(.title2).fontWeight(.semibold)
            HStack {
                TextField("Folder", text: $root).textFieldStyle(.roundedBorder)
                Button("Choose…") {
                    let p = NSOpenPanel()
                    p.canChooseDirectories = true
                    p.canChooseFiles = false
                    p.prompt = "Share"
                    if p.runModal() == .OK, let u = p.url {
                        root = u.path
                        if name.isEmpty { name = u.lastPathComponent }
                    }
                }
            }
            HStack {
                Text("Name").frame(width: 90, alignment: .leading)
                TextField("", text: $name).textFieldStyle(.roundedBorder)
            }
            HStack {
                Text("Backups").frame(width: 90, alignment: .leading)
                Picker("", selection: $backup) {
                    Text("Keep the last 10 versions of each file").tag("file:10")
                    Text("Keep the last 50 versions of each file").tag("file:50")
                    Text("Daily snapshots, keep 14").tag("time:24:14")
                    Text("None").tag("none")
                }
                .labelsHidden().frame(width: 320)
            }
            Hint("The folder stays an ordinary folder. Versions and deleted files are kept inside .medialake-sync/ next to it.")
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)) }
            HStack {
                Spacer()
                Button("Cancel") { close() }
                Button("Share") {
                    Task {
                        if await model.share(name: name, root: root, backup: backup) { close() } else { error = model.message }
                    }
                }
                .buttonStyle(.borderedProminent).disabled(root.isEmpty || name.isEmpty)
            }
        }
        .padding(20).frame(width: 520)
    }
}

private struct InviteSheet: View {
    @ObservedObject var model: SyncModel
    let close: () -> Void
    @State private var access: [String: String] = [:]
    @State private var code = ""
    @State private var error = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Invite a device").font(.title2).fontWeight(.semibold)
            Hint("Choose what the other computer may use, then give it the code. The code works once.")
            ForEach(model.hosted) { h in
                HStack {
                    Text(h.name)
                    Spacer()
                    Picker("", selection: Binding(get: { access[h.name] ?? "none" }, set: { access[h.name] = $0 })) {
                        Text("No access").tag("none")
                        Text("Read only").tag("ro")
                        Text("Read and write").tag("rw")
                    }
                    .labelsHidden().frame(width: 160)
                }
            }
            if !code.isEmpty {
                Text(code).textSelection(.enabled).font(.system(size: 12, design: .monospaced))
                    .padding(8).frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(0.07)))
                Button("Copy code") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(code, forType: .string)
                }
            }
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)) }
            HStack {
                Spacer()
                Button(code.isEmpty ? "Cancel" : "Done") { close() }
                if code.isEmpty {
                    Button("Create code") {
                        let grants = access.filter { $0.value != "none" }.map { ($0.key, $0.value) }
                        Task {
                            if let c = await model.invite(grants: grants) { code = c } else { error = model.message }
                        }
                    }
                    .buttonStyle(.borderedProminent).disabled(access.values.allSatisfy { $0 == "none" })
                }
            }
        }
        .padding(20).frame(width: 480)
    }
}

// MARK: - Browse (keep on this Mac / free up space / history)

private struct BrowseSheet: View {
    @ObservedObject var model: SyncModel
    let folder: SyncFolder
    let close: () -> Void
    @State private var path = ""
    @State private var rows: [BrowseRow] = []
    @State private var error = ""
    @State private var history: BrowseRow?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(folder.name).font(.title2).fontWeight(.semibold)
                Text("/" + path).opacity(0.6).lineLimit(1).truncationMode(.head)
                Spacer()
                Button("Done") { close() }
            }
            HStack(spacing: 10) {
                if !path.isEmpty {
                    Button { path = (path as NSString).deletingLastPathComponent; Task { await reload() } } label: { Label("Up", systemImage: "chevron.left") }
                }
                if folder.isMount {
                    Button("Keep this folder on this Mac") { Task { await pin(path) } }
                    Button("Free up space here") { Task { await unpin(path) } }
                }
            }
            .font(.system(size: 12))
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)) }
            List(rows) { r in
                HStack(spacing: 8) {
                    Image(systemName: r.isDir ? "folder" : "doc")
                    Text(r.name).lineLimit(1)
                    if r.pinned { Image(systemName: "pin.fill").font(.system(size: 10)).opacity(0.6) }
                    Spacer()
                    if !r.isDir {
                        Text(formatBytes(r.size)).font(.system(size: 12)).opacity(0.6)
                        Text(r.stateText).font(.system(size: 12)).opacity(0.6).frame(width: 150, alignment: .trailing)
                    }
                    if folder.isMount {
                        if r.pinned || r.state == "cached" || r.state == "partial" {
                            Button("Free up space") { Task { await unpin(r.path) } }.font(.system(size: 11))
                        }
                        if !r.pinned && (r.state == "cloud" || r.state == "partial" || r.isDir) {
                            Button("Keep on this Mac") { Task { await pin(r.path) } }.font(.system(size: 11))
                        }
                    }
                    if !r.isDir { Button("History") { history = r }.font(.system(size: 11)) }
                }
                .contentShape(Rectangle())
                .onTapGesture(count: 2) {
                    if r.isDir {
                        path = r.path
                        Task { await reload() }
                    }
                }
            }
            .frame(minHeight: 320)
        }
        .padding(20).frame(width: 760, height: 520)
        .task { await reload() }
        .sheet(item: $history) { r in HistorySheet(model: model, folder: folder, path: r.path) { history = nil } }
    }

    private func reload() async { rows = await model.browse(folder, path) }

    private func pin(_ p: String) async {
        error = await model.pin(folder, p) ?? ""
        await reload()
    }

    private func unpin(_ p: String) async {
        error = await model.unpin(folder, p) ?? ""
        try? await Task.sleep(nanoseconds: 400_000_000)
        await reload()
    }
}

private struct HistorySheet: View {
    @ObservedObject var model: SyncModel
    let folder: SyncFolder
    let path: String
    let close: () -> Void
    @State private var versions: [VersionRow] = []
    @State private var error = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("History of \((path as NSString).lastPathComponent)").font(.title2).fontWeight(.semibold)
            if versions.isEmpty { Text("No earlier versions are kept for this file.").opacity(0.6) }
            List(versions) { v in
                HStack {
                    Text(Date(timeIntervalSince1970: Double(v.created)).formatted(date: .abbreviated, time: .shortened))
                    Text("\(formatBytes(v.size)) · \(v.author)").font(.system(size: 12)).opacity(0.6)
                    Spacer()
                    // a restore, also as a copy, is a change on the sharing computer: read-only members cannot make one
                    Button("Restore as a copy") { Task { await restore(v, asCopy: true) } }.font(.system(size: 11)).disabled(folder.readOnly)
                    Button("Restore") { Task { await restore(v, asCopy: false) } }.font(.system(size: 11)).disabled(folder.readOnly)
                }
            }
            .frame(minHeight: 200)
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)) }
            HStack { Spacer(); Button("Done") { close() } }
        }
        .padding(20).frame(width: 600, height: 400)
        .task { versions = await model.versions(folder, path) }
    }

    private func restore(_ v: VersionRow, asCopy: Bool) async {
        if await model.restore(folder, path: path, rev: v.rev, asCopy: asCopy) { close() } else { error = model.message }
    }
}
