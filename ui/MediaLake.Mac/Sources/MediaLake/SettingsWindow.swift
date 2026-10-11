import AppKit
import SwiftUI

// MARK: - Model

struct DirItem: Identifiable, Equatable {
    var path: String
    var recursive: Bool
    var denied = false
    var id: String { path }
}

@MainActor
final class SettingsModel: ObservableObject {
    enum Page: Int, CaseIterable, Identifiable {
        case general, model, index, sync
        var id: Int { rawValue }
        var title: String { ["General", "Model", "Index", "Sync"][rawValue] }
        var symbol: String { ["gearshape", "cpu", "folder", "arrow.triangle.2.circlepath"][rawValue] }
    }

    /// Owned by the app (the status item needs it while this window is closed).
    let sync: SyncModel

    init(sync: SyncModel) { self.sync = sync }

    @Published var page: Page = .general

    // loaded settings: the object is kept whole so fields this UI does not know survive a save
    private var cfg: [String: Any] = [:]
    private var loading = true
    private var saveTask: Task<Void, Never>?

    // General
    @Published var searchEnabled = true
    /// Empty = no global shortcut.
    @Published var hotkeyText = ""
    @Published var hotkeyError = ""
    @Published var autostart = false
    @Published var autostartError = ""

    // Model
    @Published var mode = "local"
    @Published var embedModel = "embeddinggemma-2"
    @Published var modelList: [(id: String, label: String)] = []
    @Published var quality = "normal"
    /// Download state of every quality tier, per model id.
    @Published var allModels: [String: [String: (downloaded: Bool, size: Int64)]] = [:]
    var models: [String: (downloaded: Bool, size: Int64)] { allModels[embedModel] ?? [:] }
    @Published var qualityInfo = ""
    @Published var downloading = false
    @Published var downloadProgress = 0.0
    @Published var downloadText = ""
    @Published var pairCode = ""
    @Published var connecting = false
    @Published var remoteStatus = ""
    @Published var remoteQuality = ""
    @Published var threshold = 0.4
    @Published var videoEnabled = false

    // Index
    @Published var dirs: [DirItem] = []
    @Published var debounceSeconds = 2.0
    @Published var minChangeSlider = 0.0
    @Published var maxSizeSlider = 0.0
    @Published var statsText = ""

    var applyHotkey: ((String) -> String?)?
    /// Turning search on while its model is missing leads to the welcome window instead.
    var openSearchSetup: (() -> Void)?
    var requestQuit: (() -> Void)?
    var uninstall: (() -> Void)?

    private var core: CoreClient? { AppState.core }

    // MARK: load / save

    func load() async {
        guard let core else { return }
        do {
            cfg = try await core.callAsync("get_settings")
            let m = try await core.callAsync("models")
            var all: [String: [String: (downloaded: Bool, size: Int64)]] = [:]
            var list: [(id: String, label: String)] = []
            for e in (m["models"] as? [[String: Any]]) ?? [] {
                let id = e["id"] as? String ?? ""
                var map: [String: (downloaded: Bool, size: Int64)] = [:]
                for q in (e["qualities"] as? [[String: Any]]) ?? [] {
                    map[q["quality"] as? String ?? ""] = (q["downloaded"] as? Bool ?? false, (q["size"] as? NSNumber)?.int64Value ?? 0)
                }
                all[id] = map
                let media = e["supports_audio"] as? Bool == true ? "text, images, audio, video" : "text, images, video"
                list.append((id, "\(e["name"] as? String ?? id) (\(media))"))
            }
            allModels = all
            modelList = list
        } catch {
            hotkeyError = error.localizedDescription
            return
        }
        loading = true
        searchEnabled = cfg["search_enabled"] as? Bool ?? true
        hotkeyText = cfg["hotkey"] as? String ?? ""
        autostart = LoginItem.isEnabled
        mode = cfg["server_mode"] as? String ?? "local"
        embedModel = cfg["model"] as? String ?? "embeddinggemma-2"
        quality = cfg["quality"] as? String ?? "normal"
        threshold = cfg["threshold"] as? Double ?? 0.4
        videoEnabled = cfg["video_enabled"] as? Bool ?? false
        refreshQualityInfo()
        remoteQuality = "Quality is chosen by the server (currently \(quality))."
        if let r = cfg["remote"] as? [String: Any] {
            remoteStatus = "Paired with \(r["address"] as? String ?? "")"
        } else {
            remoteStatus = "Not connected"
        }
        debounceSeconds = Double((cfg["debounce_ms"] as? NSNumber)?.int64Value ?? 2000) / 1000.0
        minChangeSlider = Self.toSlider((cfg["min_change_bytes"] as? NSNumber)?.int64Value ?? 512, min: 64, max: 1_048_576, zeroIsMin: true)
        maxSizeSlider = Self.toSlider((cfg["max_file_bytes"] as? NSNumber)?.int64Value ?? (256 << 20), min: 1_048_576, max: 4_294_967_296, zeroIsMin: false)
        rebuildDirs()
        loading = false
        await refreshStats()
    }

    private func rebuildDirs() {
        let a = (cfg["dirs"] as? [[String: Any]]) ?? []
        dirs = a.map {
            let p = $0["path"] as? String ?? ""
            return DirItem(path: p, recursive: $0["recursive"] as? Bool ?? true, denied: FolderAccess.isDenied(p))
        }
    }

    private func reloadDirs() async {
        guard let core else { return }
        if let s = try? await core.callAsync("get_settings") { cfg["dirs"] = s["dirs"] }
        rebuildDirs()
        await refreshStats()
    }

    func save() async {
        guard let core, !loading else { return }
        do {
            // the Sync page and the welcome window save their own settings: do not write back an older
            // copy of them (the welcome flag would otherwise come back as false)
            if let fresh = try? await core.callAsync("get_settings") {
                cfg["sync"] = fresh["sync"]
                cfg["welcome_done"] = fresh["welcome_done"]
            }
            let res = try await core.callAsync("set_settings", cfg)
            if res["reindex"] as? Bool == true { await refreshStats() }
        } catch {
            hotkeyError = error.localizedDescription
        }
    }

    private func saveSoon() {
        guard !loading else { return }
        saveTask?.cancel()
        saveTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 500_000_000)
            if Task.isCancelled { return }
            await self?.save()
        }
    }

    func refreshStats() async {
        guard let core else { return }
        guard let st = try? await core.callAsync("status") else { return }
        let files = (st["files"] as? NSNumber)?.uint64Value ?? 0
        let vectors = (st["vectors"] as? NSNumber)?.int64Value ?? 0
        statsText = "\(files.formatted()) files · \(vectors.formatted()) vectors" + (st["busy"] as? Bool == true ? " · indexing…" : "")
    }

    func handle(event ev: [String: Any]) {
        switch ev["type"] as? String {
        case "download":
            let d = (ev["done"] as? NSNumber)?.doubleValue ?? 0
            let t = (ev["total"] as? NSNumber)?.doubleValue ?? 0
            downloadProgress = t > 0 ? d / t : 0
            downloadText = "\(Int(d / 1_048_576).formatted()) / \(Int(t / 1_048_576).formatted()) MB"
        case "idle", "progress":
            Task { await refreshStats() }
        default:
            break
        }
    }

    // MARK: General

    func hotkeyRecorded(_ combo: String) -> String? {
        let previous = cfg["hotkey"] as? String ?? ""
        if let err = applyHotkey?(combo) {
            _ = applyHotkey?(previous)
            return err
        }
        cfg["hotkey"] = combo
        Task { await save() }
        return nil
    }

    func setSearchEnabled(_ on: Bool) {
        guard !loading else { return }
        Task {
            if on, let st = try? await core?.callAsync("status"), st["model_ready"] as? Bool != true {
                // there is no model to search with: setting it up is the way to switch search on
                searchEnabled = false
                openSearchSetup?()
                return
            }
            cfg["search_enabled"] = on
            await save()
        }
    }

    func setAutostart(_ on: Bool) {
        if let err = LoginItem.set(on) {
            autostartError = err
        } else {
            autostartError = ""
        }
        autostart = LoginItem.isEnabled
    }

    func killDaemon() {
        let alert = NSAlert()
        alert.messageText = "Quit MediaLake?"
        alert.informativeText = "Stops the background process and the embedding server. The shortcut stops working until you start MediaLake again."
        alert.addButton(withTitle: "Quit")
        alert.addButton(withTitle: "Cancel")
        if alert.runModal() == .alertFirstButtonReturn { requestQuit?() }
    }

    // MARK: Model

    func modeChanged(_ new: String) {
        guard !loading else { return }
        if new == "local", (cfg["server_mode"] as? String) != "local" {
            Task {
                do {
                    _ = try await core?.callAsync("use_local")
                    cfg["server_mode"] = "local"
                } catch { remoteStatus = error.localizedDescription }
            }
        } else if new == "remote", cfg["remote"] == nil || cfg["remote"] is NSNull {
            remoteStatus = "Paste a pairing code and press Connect."
        }
    }

    func connect() async {
        let code = pairCode.trimmingCharacters(in: .whitespacesAndNewlines)
        if code.isEmpty {
            remoteStatus = "Paste the pairing code first."
            return
        }
        connecting = true
        remoteStatus = "Connecting…"
        do {
            let res = try await core!.callAsync("pair_remote", ["code": code])
            cfg["server_mode"] = "remote"
            remoteStatus = "Connected to \(res["address"] as? String ?? "") (\(res["quality"] as? String ?? ""), \(res["backend"] as? String ?? ""))"
            pairCode = ""
            if res["reindex"] as? Bool == true { remoteStatus += ". The index is being rebuilt with the server's model." }
        } catch {
            remoteStatus = error.localizedDescription
        }
        connecting = false
    }

    func refreshQualityInfo() {
        guard let m = models[quality] else { return }
        qualityInfo = (m.downloaded
            ? "Downloaded"
            : "Not downloaded yet (\((m.size / 1_048_576).formatted()) MB). MediaLake cannot index or search until the model is present")
            + ". Changing the model or the quality re-indexes everything."
    }

    var showDownloadNow: Bool { !(models[quality]?.downloaded ?? true) && !downloading }

    func downloadNow() async {
        let tag = quality
        downloading = true
        downloadProgress = 0
        do {
            _ = try await core!.callAsync("download_model", ["model": embedModel, "quality": tag])
            allModels[embedModel]?[tag] = (true, models[tag]?.size ?? 0)
        } catch {
            qualityInfo = error.localizedDescription
        }
        downloading = false
        refreshQualityInfo()
    }

    func cancelDownload() {
        Task { _ = try? await core?.callAsync("cancel_download") }
    }

    func qualityPicked(_ tag: String) async { await switchTo(modelId: embedModel, tag: tag) }

    func modelPicked(_ id: String) async { await switchTo(modelId: id, tag: quality) }

    /// Switches model and/or quality after a confirmation (and a download when the files are missing).
    private func switchTo(modelId: String, tag: String) async {
        if loading || downloading || (modelId == embedModel && tag == quality) { return }
        let modelChanged = modelId != embedModel
        let info = allModels[modelId]?[tag]
        let needsDownload = info != nil && !(info!.downloaded)
        let name = modelList.first { $0.id == modelId }?.label ?? modelId
        let alert = NSAlert()
        alert.messageText = modelChanged ? "Switch to \(name)?" : "Switch to \(tag) quality?"
        alert.informativeText = needsDownload
            ? "This needs a download of \(((info?.size ?? 0) / 1_048_576).formatted()) MB (\(tag) quality). Afterwards the whole index is rebuilt with the new model."
            : "The whole index is rebuilt with the new \(modelChanged ? "model" : "quality"). Searching keeps working while this happens."
        alert.addButton(withTitle: needsDownload ? "Download and switch" : "Switch")
        alert.addButton(withTitle: "Cancel")
        if alert.runModal() != .alertFirstButtonReturn {
            revertQuality()
            return
        }
        if needsDownload {
            downloading = true
            downloadProgress = 0
            do {
                _ = try await core!.callAsync("download_model", ["model": modelId, "quality": tag])
                allModels[modelId]?[tag] = (true, info?.size ?? 0)
            } catch {
                qualityInfo = error.localizedDescription
                downloading = false
                revertQuality()
                return
            }
            downloading = false
        }
        embedModel = modelId
        quality = tag
        cfg["model"] = modelId
        cfg["quality"] = tag
        refreshQualityInfo()
        await save()
        if modelChanged, let fresh = try? await core?.callAsync("get_settings") {
            cfg = fresh  // the score mapping follows the model
        }
    }

    private func revertQuality() {
        let (m, q) = (embedModel, quality)
        (embedModel, quality) = ("", "")  // force the pickers to re-read their bindings
        (embedModel, quality) = (m, q)
        refreshQualityInfo()
    }

    func videoToggled(_ on: Bool) {
        guard !loading else { return }
        cfg["video_enabled"] = on
        Task { await save() }
    }

    func sliderChanged() {
        guard !loading else { return }
        cfg["threshold"] = (threshold * 100).rounded() / 100
        cfg["debounce_ms"] = Int64(debounceSeconds * 1000)
        cfg["min_change_bytes"] = Self.fromSlider(minChangeSlider, min: 64, max: 1_048_576, zeroIsMin: true)
        cfg["max_file_bytes"] = Self.fromSlider(maxSizeSlider, min: 1_048_576, max: 4_294_967_296, zeroIsMin: false)
        saveSoon()
    }

    // MARK: Index

    static func toSlider(_ v: Int64, min: Double, max: Double, zeroIsMin: Bool) -> Double {
        if v <= 0 && zeroIsMin { return 0 }
        return Swift.min(Swift.max(log(Swift.max(Double(v), min) / min) / log(max / min), 0), 1)
    }

    static func fromSlider(_ s: Double, min: Double, max: Double, zeroIsMin: Bool) -> Int64 {
        if zeroIsMin && s <= 0.001 { return 0 }
        return Int64((min * pow(max / min, s)).rounded())
    }

    static func bytes(_ b: Int64) -> String {
        switch b {
        case 0: return "0 B (re-embed on every change)"
        case ..<1024: return "\(b) B"
        case ..<(1 << 20): return String(format: "%.1f KB", Double(b) / 1024)
        case ..<(1 << 30): return String(format: "%.1f MB", Double(b) / 1_048_576)
        default: return String(format: "%.1f GB", Double(b) / 1_073_741_824)
        }
    }

    var debounceLabel: String { String(format: "%.1f seconds after the last change", debounceSeconds) }
    var minChangeLabel: String {
        "\(Self.bytes(Self.fromSlider(minChangeSlider, min: 64, max: 1_048_576, zeroIsMin: true))). Smaller edits are collected until they add up."
    }
    var maxSizeLabel: String {
        "\(Self.bytes(Self.fromSlider(maxSizeSlider, min: 1_048_576, max: 4_294_967_296, zeroIsMin: false))). Larger files are searchable by name only."
    }

    func addFolder() async {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            _ = try await core!.callAsync("add_dir", ["path": url.path, "recursive": true])
            await reloadDirs()
        } catch { statsText = error.localizedDescription }
    }

    func removeFolder(_ path: String) async {
        do {
            _ = try await core!.callAsync("remove_dir", ["path": path])
            await reloadDirs()
        } catch { statsText = error.localizedDescription }
    }

    func setRecursive(_ path: String, _ on: Bool) async {
        do {
            _ = try await core!.callAsync("add_dir", ["path": path, "recursive": on])
            if let s = try? await core!.callAsync("get_settings") { cfg["dirs"] = s["dirs"] }
        } catch { statsText = error.localizedDescription }
    }

    func reindexAll() async {
        let alert = NSAlert()
        alert.messageText = "Reindex everything?"
        alert.informativeText = "All entries are rebuilt from your files. Searching keeps working while this happens."
        alert.addButton(withTitle: "Reindex")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        do {
            _ = try await core!.callAsync("reindex_all")
            await refreshStats()
        } catch { statsText = error.localizedDescription }
    }
}

// MARK: - Window

@MainActor
final class SettingsWindowController: NSObject, NSWindowDelegate {
    let model: SettingsModel
    let window: NSWindow
    var onClose: (() -> Void)?
    var rootView: NSView { window.contentView! }

    init(sync: SyncModel) {
        model = SettingsModel(sync: sync)
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 960, height: 720),
            styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        super.init()
        window.title = "MediaLake Settings"
        window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: SettingsView(model: model))
        window.center()
        window.delegate = self
    }

    func show() {
        Task { await model.load() }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func windowWillClose(_ notification: Notification) { onClose?() }
}

// MARK: - Views

struct SettingsView: View {
    @ObservedObject var model: SettingsModel

    var body: some View {
        NavigationSplitView {
            List(SettingsModel.Page.allCases, selection: $model.page) { p in
                Label(p.title, systemImage: p.symbol).tag(p)
            }
            .navigationSplitViewColumnWidth(190)
        } detail: {
            ScrollView {
                Group {
                    switch model.page {
                    case .general: GeneralPage(model: model)
                    case .model: ModelPage(model: model)
                    case .index: IndexPage(model: model)
                    case .sync: SyncPage(model: model.sync)
                    }
                }
                .padding(.horizontal, 32).padding(.vertical, 24)
                .frame(maxWidth: 760, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .onChange(of: model.page) { _, p in
            if p == .index { Task { await model.refreshStats() } }
            if p == .sync { Task { await model.sync.refresh() } }
        }
    }
}

struct PageTitle: View {
    let text: String
    var body: some View { Text(text).font(.title).fontWeight(.semibold) }
}

struct Hint: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View { Text(text).opacity(0.7).fixedSize(horizontal: false, vertical: true) }
}

struct GeneralPage: View {
    @ObservedObject var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            PageTitle(text: "General")
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Search my files by content", isOn: Binding(get: { model.searchEnabled }, set: { v in
                    model.searchEnabled = v
                    model.setSearchEnabled(v)
                }))
                Hint("Turn this off to use MediaLake only for syncing: no search model is loaded and nothing is indexed.")
            }
            VStack(alignment: .leading, spacing: 6) {
                Text("Global shortcut (optional)").fontWeight(.semibold)
                Hint("Press this shortcut anywhere to open the search window. Without one, use the menu bar icon.")
                HotkeyRecorder(text: $model.hotkeyText, error: $model.hotkeyError) { model.hotkeyRecorded($0) }
            }
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Start MediaLake when I sign in", isOn: Binding(get: { model.autostart }, set: { model.setAutostart($0) }))
                if !model.autostartError.isEmpty { Text(model.autostartError).foregroundColor(.red).font(.system(size: 12)) }
            }
            VStack(alignment: .leading, spacing: 8) {
                Text("Maintenance").fontWeight(.semibold)
                HStack(spacing: 12) {
                    Button("Kill daemon") { model.killDaemon() }.help("Stop MediaLake and its embedding server")
                    Button("Uninstall") { model.uninstall?() }
                }
            }
        }
    }
}

struct ModelPage: View {
    @ObservedObject var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            PageTitle(text: "Model")
            VStack(alignment: .leading, spacing: 6) {
                Text("Where should embeddings be computed?")
                Picker("", selection: $model.mode) {
                    Text("On this computer (bundled server)").tag("local")
                    Text("On a remote server").tag("remote")
                }
                .pickerStyle(.radioGroup)
                .labelsHidden()
                .onChange(of: model.mode) { _, m in model.modeChanged(m) }
            }

            if model.mode == "local" {
                VStack(alignment: .leading, spacing: 14) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Model")
                        Picker("", selection: Binding(get: { model.embedModel }, set: { v in Task { await model.modelPicked(v) } })) {
                            ForEach(model.modelList, id: \.id) { m in Text(m.label).tag(m.id) }
                        }
                        .labelsHidden().frame(width: 360, alignment: .leading).disabled(model.downloading)
                    }
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Quality")
                        Picker("", selection: Binding(get: { model.quality }, set: { v in Task { await model.qualityPicked(v) } })) {
                            Text("Heavy: most precise, largest").tag("heavy")
                            Text("Normal: recommended").tag("normal")
                            Text("Light: smallest and fastest").tag("light")
                        }
                        .labelsHidden().frame(width: 360, alignment: .leading).disabled(model.downloading)
                        Hint(model.qualityInfo)
                        if model.showDownloadNow {
                            Button("Download now") { Task { await model.downloadNow() } }.buttonStyle(.borderedProminent)
                        }
                        if model.downloading {
                            ProgressView(value: model.downloadProgress).progressViewStyle(.linear)
                            Text(model.downloadText).opacity(0.7)
                            Button("Cancel download") { model.cancelDownload() }
                        }
                    }
                }
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Pairing code")
                    TextField("SN1-…  (run \"medialake-server pair\" on the server)", text: $model.pairCode)
                        .textFieldStyle(.roundedBorder)
                    HStack(spacing: 12) {
                        Button("Connect") { Task { await model.connect() } }
                            .buttonStyle(.borderedProminent).disabled(model.connecting)
                        Text(model.remoteStatus).opacity(0.8).fixedSize(horizontal: false, vertical: true)
                    }
                    Text(model.remoteQuality).opacity(0.7)
                    HStack(alignment: .top, spacing: 8) {
                        Image(systemName: "info.circle")
                        Text("With a remote server, the content of your indexed files is sent to that server for embedding. The connection is encrypted and pinned to the server's certificate.")
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(10)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color.accentColor.opacity(0.12)))
                }
            }

            VStack(alignment: .leading, spacing: 4) {
                Text("Confidence threshold").fontWeight(.semibold)
                Hint("Results below this confidence are hidden.")
                Slider(value: $model.threshold, in: 0...1, step: 0.01) { _ in model.sliderChanged() }
                    .frame(width: 360)
                    .onChange(of: model.threshold) { _, _ in model.sliderChanged() }
            }

            DisclosureGroup("Experimental") {
                VStack(alignment: .leading, spacing: 8) {
                    Toggle("Embed video content", isOn: $model.videoEnabled)
                        .onChange(of: model.videoEnabled) { _, on in model.videoToggled(on) }
                    Hint("Samples up to 8 frames per video. Slow and CPU-heavy: turning this on queues all videos for content embedding; turning it off removes their content vectors. Videos are always searchable by name.")
                }
                .padding(.top, 6)
            }
        }
    }
}

struct IndexPage: View {
    @ObservedObject var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            PageTitle(text: "Index")
            VStack(alignment: .leading, spacing: 8) {
                Text("Indexed folders").fontWeight(.semibold)
                VStack(spacing: 0) {
                    if model.dirs.isEmpty {
                        Text("No folders yet").opacity(0.6).frame(maxWidth: .infinity, minHeight: 40)
                    }
                    ForEach(model.dirs) { d in
                        VStack(alignment: .leading, spacing: 2) {
                            HStack(spacing: 12) {
                                Text(d.path).lineLimit(1).truncationMode(.middle).frame(maxWidth: .infinity, alignment: .leading)
                                Toggle("Include subfolders", isOn: Binding(
                                    get: { d.recursive },
                                    set: { v in
                                        if let i = model.dirs.firstIndex(where: { $0.path == d.path }) { model.dirs[i].recursive = v }
                                        Task { await model.setRecursive(d.path, v) }
                                    }))
                                Button { Task { await model.removeFolder(d.path) } } label: { Image(systemName: "trash") }
                                    .help("Remove")
                            }
                            if d.denied {
                                HStack(spacing: 8) {
                                    Text("No access: macOS did not allow MediaLake to read this folder.")
                                        .font(.system(size: 12)).foregroundColor(.red)
                                    Button("Open Privacy settings…") { ShellActions.openFilesAndFoldersSettings() }
                                        .font(.system(size: 12))
                                }
                            }
                        }
                        .padding(.vertical, 4)
                    }
                }
                .frame(maxHeight: 240)
                Button("Add folder…") { Task { await model.addFolder() } }
            }
            sliderBlock("Wait after a file is modified", model.debounceLabel) {
                Slider(value: $model.debounceSeconds, in: 0.5...60, step: 0.5)
                    .onChange(of: model.debounceSeconds) { _, _ in model.sliderChanged() }
            }
            sliderBlock("Minimum change to re-embed", model.minChangeLabel) {
                Slider(value: $model.minChangeSlider, in: 0...1, step: 0.01)
                    .onChange(of: model.minChangeSlider) { _, _ in model.sliderChanged() }
            }
            sliderBlock("Maximum file size to index", model.maxSizeLabel) {
                Slider(value: $model.maxSizeSlider, in: 0...1, step: 0.01)
                    .onChange(of: model.maxSizeSlider) { _, _ in model.sliderChanged() }
            }
            VStack(alignment: .leading, spacing: 8) {
                Text(model.statsText).opacity(0.8)
                Button("Reindex all") { Task { await model.reindexAll() } }
            }
        }
    }

    private func sliderBlock<S: View>(_ title: String, _ label: String, @ViewBuilder slider: () -> S) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).fontWeight(.semibold)
            Hint(label)
            slider().frame(width: 360)
        }
    }
}

// MARK: - Hotkey recorder

/// Click, then press the combination. Registering alone is not a reliable conflict check on macOS, so
/// known system shortcuts are rejected up front.
struct HotkeyRecorder: View {
    @Binding var text: String
    @Binding var error: String
    let apply: (String) -> String?

    @State private var recording = false
    @State private var monitor: Any?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 12) {
                Text(recording ? "…" : (text.isEmpty ? "Not set" : HotkeySpec.display(of: text)))
                    .opacity(text.isEmpty && !recording ? 0.55 : 1)
                    .frame(minWidth: 180, alignment: .leading)
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(recording ? 0.14 : 0.07)))
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(recording ? Color.accentColor : Color.primary.opacity(0.25)))
                    .contentShape(Rectangle())
                    .onTapGesture { startRecording() }
                Text(recording ? "Press the new combination… (Esc cancels)" : "Click, then press the new combination")
                    .opacity(0.6)
                if !text.isEmpty && !recording {
                    Button("Remove") {
                        if let err = apply("") {
                            error = err
                        } else {
                            error = ""
                            text = ""
                        }
                    }
                }
            }
            if !error.isEmpty { Text(error).foregroundColor(.red).font(.system(size: 12)) }
        }
        .onDisappear { stopRecording() }
    }

    private func startRecording() {
        guard !recording else { return }
        recording = true
        error = ""
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown]) { ev in
            if ev.keyCode == 53 {  // Esc
                stopRecording()
                return nil
            }
            guard let spec = HotkeySpec.from(event: ev) else { return nil }
            if spec.modifiers == 0 {
                error = "Include at least one of Ctrl, Alt, Shift or Cmd."
                return nil
            }
            if let who = spec.reservedBy {
                error = "\(spec.display) is used by macOS for \(who). Pick another combination."
                return nil
            }
            if let err = apply(spec.text) {
                error = err
            } else {
                error = ""
                text = spec.text
            }
            stopRecording()
            return nil
        }
    }

    private func stopRecording() {
        recording = false
        if let m = monitor { NSEvent.removeMonitor(m) }
        monitor = nil
    }
}
