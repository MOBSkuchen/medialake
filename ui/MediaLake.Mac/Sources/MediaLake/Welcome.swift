import AppKit
import SwiftUI

// MARK: - Model

struct WelcomeQuality: Identifiable, Hashable {
    let name: String
    let size: Int64
    let downloaded: Bool
    var id: String { name }

    var label: String {
        let l: String
        switch name {
        case "heavy": l = "Heavy: most precise"
        case "light": l = "Light: smallest and fastest"
        default: l = "Normal: recommended"
        }
        return "\(l)  ·  " + (downloaded ? "on this Mac" : formatSize(size))
    }
}

struct WelcomeModelChoice: Identifiable, Hashable {
    let id: String
    let label: String
    let qualities: [WelcomeQuality]
}

/// The "Welcome to MediaLake" window: page 1 sets up search (shortcut and model, both optional), page 2
/// sync (optional too). Nothing is applied before a button is pressed: *Skip for now* sets up no shortcut
/// and downloads no model.
@MainActor
final class WelcomeModel: ObservableObject {
    enum Page { case search, sync }

    @Published var page: Page = .search

    // search
    @Published var models: [WelcomeModelChoice] = []
    @Published var modelId = ""
    @Published var quality = "normal"
    /// Staged: registered and saved only when Download is pressed. Empty = no shortcut.
    @Published var shortcut = ""
    @Published var shortcutError = ""
    @Published var autostart = false
    @Published var autostartError = ""
    @Published var downloading = false
    @Published var progress: Double?
    @Published var info = ""
    /// Search is on and its model is on this computer.
    @Published var searchReady = false

    // sync
    let sync: SyncModel

    /// Registers the shortcut ("" = release it). Returns an error text or nil.
    var applyHotkey: ((String) -> String?)?
    /// Search state changed: the search window re-reads it.
    var onSearchChanged: (() -> Void)?
    /// The window should go away.
    var onClose: (() -> Void)?

    private var core: CoreClient? { AppState.core }

    init(sync: SyncModel) { self.sync = sync }

    var currentQualities: [WelcomeQuality] { models.first { $0.id == modelId }?.qualities ?? [] }
    var selectedDownloaded: Bool { currentQualities.first { $0.name == quality }?.downloaded ?? false }

    // MARK: load

    func load() async {
        guard let core else { return }
        do {
            let cfg = try await core.callAsync("get_settings")
            let m = try await core.callAsync("models")
            var list: [WelcomeModelChoice] = []
            for e in (m["models"] as? [[String: Any]]) ?? [] {
                let id = e["id"] as? String ?? ""
                let qs = ((e["qualities"] as? [[String: Any]]) ?? []).map {
                    WelcomeQuality(name: $0["quality"] as? String ?? "", size: ($0["size"] as? NSNumber)?.int64Value ?? 0,
                                   downloaded: $0["downloaded"] as? Bool ?? false)
                }
                list.append(WelcomeModelChoice(id: id, label: e["name"] as? String ?? id, qualities: qs))
            }
            models = list
            if !downloading {
                modelId = cfg["model"] as? String ?? list.first?.id ?? ""
                quality = cfg["quality"] as? String ?? "normal"
            }
            shortcut = cfg["hotkey"] as? String ?? ""
            autostart = LoginItem.isEnabled
            let st = try await core.callAsync("status")
            searchReady = st["search"] as? String == "ready"
            downloading = st["downloading"] as? Bool ?? downloading
        } catch {
            info = error.localizedDescription
        }
    }

    func handle(event ev: [String: Any]) {
        guard ev["type"] as? String == "download" else { return }
        let d = (ev["done"] as? NSNumber)?.doubleValue ?? 0
        let t = (ev["total"] as? NSNumber)?.doubleValue ?? 0
        downloading = true
        progress = t > 0 ? d / t : 0
        info = "\(formatSize(Int64(d))) of \(formatSize(Int64(t)))"
    }

    /// Test hook: shows the page as it looks while a download runs.
    func previewDownload(_ fraction: Double) {
        downloading = true
        progress = fraction
        info = "\(formatSize(Int64(fraction * 1_200_000_000))) of \(formatSize(1_200_000_000))"
    }

    // MARK: page 1

    func modelPicked(_ id: String) {
        modelId = id
        if !currentQualities.contains(where: { $0.name == quality }) { quality = currentQualities.first?.name ?? "normal" }
    }

    func setAutostart(_ on: Bool) {
        if let err = LoginItem.set(on) {
            autostartError = err
        } else {
            autostartError = ""
        }
        autostart = LoginItem.isEnabled
    }

    /// Download (or use what is already here), then page 2 while it runs in the background.
    func download() async {
        guard let core, !downloading else { return }
        shortcutError = ""
        info = ""
        let combo = shortcut.trimmingCharacters(in: .whitespaces)
        if let err = applyHotkey?(combo) {
            shortcutError = err
            return
        }
        do {
            var cfg = try await core.callAsync("get_settings")
            cfg["hotkey"] = combo
            cfg["model"] = modelId
            cfg["quality"] = quality
            _ = try await core.callAsync("set_settings", cfg)
        } catch {
            info = error.localizedDescription
            return
        }
        let (model, tag) = (modelId, quality)
        page = .sync
        if !selectedDownloaded {
            downloading = true
            progress = 0
            info = "Starting download…"
        }
        onSearchChanged?()
        do {
            _ = try await core.callAsync("download_model", ["model": model, "quality": tag])
            // search is switched on only now: nothing starts a server for a model that is not there
            var cfg = try await core.callAsync("get_settings")
            cfg["search_enabled"] = true
            _ = try await core.callAsync("set_settings", cfg)
            info = ""
        } catch {
            info = error.localizedDescription
        }
        downloading = false
        progress = nil
        await load()
        onSearchChanged?()
    }

    /// No shortcut, no download. Search stays off until it is set up (the search window says so).
    func skipSearch() async {
        guard let core else { return }
        shortcutError = ""
        if !searchReady {
            do {
                var cfg = try await core.callAsync("get_settings")
                cfg["search_enabled"] = false
                _ = try await core.callAsync("set_settings", cfg)
            } catch {
                info = error.localizedDescription
            }
        }
        page = .sync
        onSearchChanged?()
    }

    // MARK: page 2 / leaving

    /// Sync has to be on before anything can be paired or joined.
    func enableSync() async -> Bool {
        if !sync.enabled { await sync.setEnabled(true) }
        return sync.enabled
    }

    /// The window is dealt with, however it ended: it is not shown again by itself.
    func finish() async {
        guard let core else { return }
        if var cfg = try? await core.callAsync("get_settings"), cfg["welcome_done"] as? Bool != true {
            cfg["welcome_done"] = true
            _ = try? await core.callAsync("set_settings", cfg)
        }
    }
}

// MARK: - Window

@MainActor
final class WelcomeWindowController: NSObject, NSWindowDelegate {
    let model: WelcomeModel
    let window: NSWindow
    var rootView: NSView { window.contentView! }
    var onClosed: (() -> Void)?

    init(sync: SyncModel) {
        model = WelcomeModel(sync: sync)
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 600), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        window.title = "MediaLake"
        window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: WelcomeView(model: model))
        window.center()
        window.delegate = self
        model.onClose = { [weak self] in self?.window.close() }
    }

    func show(page: WelcomeModel.Page = .search) {
        model.page = page
        Task { await model.load() }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func windowWillClose(_ notification: Notification) {
        Task { await model.finish() }
        onClosed?()
    }
}

// MARK: - Views

struct WelcomeView: View {
    @ObservedObject var model: WelcomeModel
    @State private var sheet: Sheet?

    enum Sheet: Identifiable {
        case join, share
        var id: Int { self == .join ? 1 : 2 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Welcome to MediaLake").font(.system(size: 26, weight: .bold)).padding(.bottom, 4)
            Text("Everything on these two pages is optional, and you can change it later in Settings.")
                .opacity(0.7).padding(.bottom, 18)
            switch model.page {
            case .search: SearchStep(model: model)
            case .sync: SyncStep(model: model, sheet: $sheet)
            }
        }
        .padding(28)
        .frame(width: 600, height: 600, alignment: .topLeading)
        .sheet(item: $sheet) { s in
            switch s {
            case .join: JoinSheet(model: model.sync) { sheet = nil }
            case .share: ShareSheet(model: model.sync) { sheet = nil }
            }
        }
    }
}

private struct PageDots: View {
    let page: WelcomeModel.Page
    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(page == .search ? Color.accentColor : Color.primary.opacity(0.25)).frame(width: 7, height: 7)
            Circle().fill(page == .sync ? Color.accentColor : Color.primary.opacity(0.25)).frame(width: 7, height: 7)
        }
    }
}

/// What a running download looks like, on both pages.
private struct DownloadFooter: View {
    @ObservedObject var model: WelcomeModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if model.downloading {
                ProgressView(value: model.progress ?? 0).progressViewStyle(.linear)
                Text("Downloading the search model… \(model.info)").font(.system(size: 12)).opacity(0.7)
                Text("You can close this window: the download continues in the background.").font(.system(size: 12)).opacity(0.5)
            } else if model.searchReady {
                Label("Search is ready. Add the folders to search in Settings > Index.", systemImage: "checkmark.circle.fill")
                    .font(.system(size: 12)).foregroundColor(.green)
            } else if !model.info.isEmpty {
                Text(model.info).font(.system(size: 12)).foregroundColor(.red).fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct SearchStep: View {
    @ObservedObject var model: WelcomeModel

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Search").font(.title3).fontWeight(.semibold)
            Text("MediaLake understands your files with an AI model that runs on this computer. It is downloaded once and stays here.")
                .opacity(0.8).fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 8) {
                Text("Model").fontWeight(.semibold)
                if model.models.count > 1 {
                    Picker("", selection: Binding(get: { model.modelId }, set: { model.modelPicked($0) })) {
                        ForEach(model.models) { m in Text(m.label).tag(m.id) }
                    }
                    .labelsHidden().frame(maxWidth: 460, alignment: .leading).disabled(model.downloading)
                }
                Picker("", selection: $model.quality) {
                    ForEach(model.currentQualities) { q in Text(q.label).tag(q.name) }
                }
                .labelsHidden().frame(maxWidth: 460, alignment: .leading).disabled(model.downloading)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Keyboard shortcut (optional)").fontWeight(.semibold)
                HotkeyRecorder(text: $model.shortcut, error: $model.shortcutError) { _ in nil }
                Hint("Press it anywhere to open the search window. Without one, open MediaLake from its menu bar icon.")
            }

            VStack(alignment: .leading, spacing: 4) {
                Toggle("Start MediaLake when I sign in", isOn: Binding(get: { model.autostart }, set: { model.setAutostart($0) }))
                if !model.autostartError.isEmpty { Text(model.autostartError).foregroundColor(.red).font(.system(size: 12)) }
            }

            Spacer(minLength: 0)
            DownloadFooter(model: model)
            HStack {
                PageDots(page: .search)
                Spacer()
                if model.downloading {
                    Button("Next") { model.page = .sync }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                } else {
                    Button("Skip for now") { Task { await model.skipSearch() } }
                    Button(model.selectedDownloaded ? "Continue" : "Download") { Task { await model.download() } }
                        .buttonStyle(.borderedProminent)
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.models.isEmpty)
                }
            }
        }
    }
}

private struct SyncStep: View {
    @ObservedObject var model: WelcomeModel
    @ObservedObject var sync: SyncModel
    @Binding var sheet: WelcomeView.Sheet?

    init(model: WelcomeModel, sheet: Binding<WelcomeView.Sheet?>) {
        self.model = model
        self.sync = model.sync
        self._sheet = sheet
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Sync").font(.title3).fontWeight(.semibold)
            Text("Keep folders in step between your computers. A synced folder appears under ~/MediaLake, lists everything at once and loads a file when you open it.")
                .opacity(0.8).fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 10) {
                Button("Add a folder from another computer…") { Task { if await model.enableSync() { sheet = .join } } }
                Hint("You need an invitation code from the computer that shares the folder (\"Invite a device\" in its Sync settings).")
                Button("Share a folder from this Mac…") { Task { if await model.enableSync() { sheet = .share } } }
            }

            if !sync.folders.isEmpty || !sync.hosted.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(sync.folders) { f in Label("\(f.name) is synced to \(f.path)", systemImage: "checkmark.circle.fill").foregroundColor(.green) }
                    ForEach(sync.hosted) { h in Label("\(h.name) is shared from this Mac", systemImage: "checkmark.circle.fill").foregroundColor(.green) }
                }
                .font(.system(size: 12))
            }
            if !sync.message.isEmpty { Text(sync.message).foregroundColor(.red).font(.system(size: 12)) }

            Spacer(minLength: 0)
            DownloadFooter(model: model)
            HStack {
                Button("Back") { model.page = .search }
                PageDots(page: .sync)
                Spacer()
                let used = !sync.folders.isEmpty || !sync.hosted.isEmpty
                Button(used ? "Done" : "Skip for now") { model.onClose?() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            }
        }
    }
}
