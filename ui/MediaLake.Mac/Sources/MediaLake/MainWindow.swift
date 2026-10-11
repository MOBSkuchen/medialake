import AppKit
import Combine
import QuickLookThumbnailing
import SwiftUI

// MARK: - Result row model

@MainActor
final class ResultItem: ObservableObject, Identifiable {
    let id = UUID()
    let path: String
    let name: String
    let folder: String
    let snippet: String
    let modality: String
    let scoreText: String
    @Published var thumb: NSImage?

    init(json r: [String: Any]) {
        path = r["path"] as? String ?? ""
        name = r["name"] as? String ?? ""
        folder = ResultItem.shorten((path as NSString).deletingLastPathComponent)
        snippet = r["snippet"] as? String ?? ""
        modality = r["modality"] as? String ?? "other"
        scoreText = String(format: "%.0f%%", ((r["score"] as? Double) ?? 0) * 100)
    }

    var badgeColor: Color {
        switch modality {
        case "text": return Color(red: 0x2B / 255, green: 0x7C / 255, blue: 0xD3 / 255)
        case "document": return Color(red: 0xC7 / 255, green: 0x70 / 255, blue: 0x1F / 255)
        case "image": return Color(red: 0x2E / 255, green: 0x9E / 255, blue: 0x5B / 255)
        case "audio": return Color(red: 0x8E / 255, green: 0x5B / 255, blue: 0xD0 / 255)
        case "video": return Color(red: 0xD1 / 255, green: 0x4B / 255, blue: 0x64 / 255)
        default: return Color(red: 0x6B / 255, green: 0x72 / 255, blue: 0x80 / 255)
        }
    }

    var symbol: String {
        switch modality {
        case "image": return "photo"
        case "audio": return "waveform"
        case "video": return "film"
        case "text": return "doc.plaintext"
        case "document": return "doc.richtext"
        default: return "doc"
        }
    }

    private static func shorten(_ p: String) -> String {
        let maxLen = 64
        if p.count <= maxLen { return p }
        let parts = p.split(separator: "/", omittingEmptySubsequences: true)
        if parts.count <= 3 { return p }
        return "/" + parts[0] + "/…/" + parts.suffix(2).joined(separator: "/")
    }

    func loadThumbnail() {
        let url = URL(fileURLWithPath: path)
        let req = QLThumbnailGenerator.Request(fileAt: url, size: CGSize(width: 44, height: 44), scale: 2, representationTypes: .all)
        Task { [weak self] in
            let image: NSImage
            if let rep = try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: req) {
                image = rep.nsImage
            } else {
                image = NSWorkspace.shared.icon(forFile: url.path)
            }
            self?.thumb = image
        }
    }
}

// MARK: - View model

struct QualityOption: Identifiable, Hashable {
    let name: String
    let size: Int64
    var id: String { name }

    var label: String {
        let l: String
        switch name {
        case "heavy": l = "Heavy: most precise"
        case "light": l = "Light: smallest and fastest"
        default: l = "Normal: recommended"
        }
        return "\(l)  ·  \(formatSize(size))"
    }
}

func formatSize(_ bytes: Int64) -> String {
    bytes >= 1 << 30 ? String(format: "%.1f GB", Double(bytes) / 1_073_741_824.0) : String(format: "%.0f MB", Double(bytes) / 1_048_576.0)
}

func formatEta(_ secs: UInt64) -> String {
    if secs < 5 { return "a few seconds" }
    if secs < 90 { return "~\(secs) s" }
    let min = (secs + 30) / 60
    if min < 90 { return "~\(min) min" }
    return "~\(min / 60) h \(min % 60) min"
}

@MainActor
final class SearchModel: ObservableObject {
    @Published var query = ""
    @Published var items: [ResultItem] = []
    @Published var selection = 0
    @Published var loading = false
    @Published var emptyText = ""
    @Published var resultsVisible = false
    @Published var focusTick = 0

    @Published var setupVisible = false
    @Published var qualities: [QualityOption] = []
    @Published var setupQuality = "normal"
    @Published var setupInfo = ""
    @Published var setupProgress: Double?
    @Published var downloading = false
    @Published var hotkeyText = "Alt+Space"
    @Published var hotkeyError = ""
    @Published var autostart = false
    @Published var autostartError = ""
    /// The model finished downloading while the setup window was open.
    @Published var setupDone = false

    @Published var progressText = ""
    @Published var serverText = ""
    @Published var indexFraction: Double?

    var currentQuality = "normal"
    var onHide: (() -> Void)?
    var onOpenSettings: (() -> Void)?
    /// First run: the search model is missing, so the setup window replaces the search panel.
    var onNeedSetup: (() -> Void)?
    var onCloseSetup: (() -> Void)?
    var applyHotkey: ((String) -> String?)?

    private var searchId = 0
    private var debounce: Task<Void, Never>?
    private var core: CoreClient? { AppState.core }

    var wantsExpanded: Bool { !query.trimmingCharacters(in: .whitespaces).isEmpty }

    var statusText: String { [serverText, progressText].filter { !$0.isEmpty }.joined(separator: "  ·  ") }

    // MARK: search

    func queryChanged() {
        debounce?.cancel()
        searchId += 1
        let id = searchId
        let q = query.trimmingCharacters(in: .whitespaces)
        if setupVisible { return }
        if q.isEmpty {
            items = []
            showEmpty("")
            loading = false
            return
        }
        loading = true
        debounce = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 150_000_000)
            if Task.isCancelled { return }
            await self?.runSearch(q, id: id)
        }
    }

    private func runSearch(_ q: String, id: Int) async {
        guard let core else {
            showEmpty("Starting…")
            return
        }
        do {
            let res = try await core.callAsync("search", ["query": q])
            if id != searchId { return }  // a newer search superseded this one
            let rows = (res["results"] as? [[String: Any]]) ?? []
            let list = rows.map { ResultItem(json: $0) }
            items = list
            list.forEach { $0.loadThumbnail() }
            selection = 0
            showEmpty(list.isEmpty ? "No matches. Try different words, or add folders in Settings." : nil)
        } catch {
            if id == searchId {
                items = []
                showEmpty(error.localizedDescription)
            }
        }
        if id == searchId { loading = false }
    }

    private func showEmpty(_ text: String?) {
        emptyText = text ?? ""
        resultsVisible = text == nil && !setupVisible
    }

    func moveSelection(_ delta: Int) {
        guard !items.isEmpty else { return }
        selection = min(max(selection + delta, 0), items.count - 1)
    }

    func openSelected(showInFinder: Bool) {
        guard items.indices.contains(selection) else { return }
        let path = items[selection].path
        if showInFinder { ShellActions.showInFinder(path) } else { ShellActions.open(path) }
        onHide?()
    }

    func open(_ item: ResultItem, showInFinder: Bool) {
        if showInFinder { ShellActions.showInFinder(item.path) } else { ShellActions.open(item.path) }
        onHide?()
    }

    // MARK: first run

    func checkSetup() async {
        guard let core, !downloading else { return }
        do {
            let cfg = try await core.callAsync("get_settings")
            let models = try await core.callAsync("models")
            let mode = cfg["server_mode"] as? String
            currentQuality = cfg["quality"] as? String ?? "normal"
            hotkeyText = cfg["hotkey"] as? String ?? hotkeyText
            let list = (models["qualities"] as? [[String: Any]]) ?? []
            qualities = list.map { QualityOption(name: $0["quality"] as? String ?? "", size: ($0["size"] as? NSNumber)?.int64Value ?? 0) }
            if !qualities.contains(where: { $0.name == setupQuality }) || !setupVisible { setupQuality = currentQuality }
            let current = list.first { ($0["quality"] as? String) == currentQuality }
            let missing = mode == "local" && current != nil && (current?["downloaded"] as? Bool) == false
            setSetup(missing)
            autostart = LoginItem.isEnabled
        } catch {
            // the status line shows core errors
        }
    }

    private func setSetup(_ visible: Bool) {
        setupVisible = visible
        if visible {
            resultsVisible = false
            emptyText = ""
        }
    }

    func download() async {
        guard let core else { return }
        let quality = setupQuality
        downloading = true
        setupProgress = 0
        setupInfo = "Starting download…"
        do {
            if quality != currentQuality {
                var cfg = try await core.callAsync("get_settings")
                cfg["quality"] = quality
                _ = try await core.callAsync("set_settings", cfg)
                currentQuality = quality
            }
            _ = try await core.callAsync("download_model", ["quality": quality])
            setupInfo = ""
        } catch {
            setupInfo = error.localizedDescription
        }
        downloading = false
        setupProgress = nil
        await checkSetup()
        if !setupVisible && setupInfo.isEmpty {
            serverText = "Model ready. Add folders to index in Settings."
            setupDone = true
            focusTick += 1
        }
    }

    func setAutostart(_ on: Bool) {
        if let err = LoginItem.set(on) {
            autostartError = err
            autostart = LoginItem.isEnabled
        } else {
            autostartError = ""
            autostart = on
        }
    }

    // MARK: core events

    func handle(event ev: [String: Any]) {
        switch ev["type"] as? String {
        case "progress":
            let queued = (ev["queued"] as? NSNumber)?.uint64Value ?? 0
            let total = (ev["total"] as? NSNumber)?.uint64Value ?? 0
            let done = (ev["done"] as? NSNumber)?.uint64Value ?? 0
            if total > 0 && (queued > 0 || done < total) {
                var text = "Indexing \(done.formatted()) / \(total.formatted())"
                if let eta = (ev["eta_secs"] as? NSNumber)?.uint64Value { text += "  ·  \(formatEta(eta)) left" }
                progressText = text
                indexFraction = Double(done) / Double(total)
            } else {
                progressText = ""
                indexFraction = nil
            }
        case "idle":
            progressText = "\(((ev["files"] as? NSNumber)?.uint64Value ?? 0).formatted()) files indexed"
            indexFraction = nil
        case "server":
            let detail = ev["detail"] as? String ?? ""
            switch ev["state"] as? String {
            case "starting": serverText = "Model starting…"
            case "failed": serverText = setupVisible ? "" : (detail.isEmpty ? "Embedding server failed" : detail)
            default: serverText = ""
            }
        case "download":
            let d = (ev["done"] as? NSNumber)?.doubleValue ?? 0
            let t = (ev["total"] as? NSNumber)?.doubleValue ?? 0
            setupProgress = t > 0 ? d / t : 0
            setupInfo = "\(formatSize(Int64(d))) of \(formatSize(Int64(t)))  ·  \(String(format: "%.0f", t > 0 ? d / t * 100 : 0))%"
        case "error":
            if !setupVisible { serverText = ev["message"] as? String ?? "" }
        default:
            break
        }
    }
}

// MARK: - Panel

/// Borderless panel that can take keyboard input without activating the app (Spotlight-style).
final class SearchPanel: NSPanel {
    var keepOpen = false
    var onResign: (() -> Void)?

    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }

    override func resignKey() {
        super.resignKey()
        if !keepOpen { onResign?() }
    }

    override func cancelOperation(_ sender: Any?) { onResign?() }
}

@MainActor
final class MainWindowController: NSObject {
    static let collapsedHeight: CGFloat = 188  // title + input + status
    static let expandedHeight: CGFloat = 580
    static let width: CGFloat = 760

    let model = SearchModel()
    let panel: SearchPanel
    private var bag = Set<AnyCancellable>()
    private var everShown = false

    var isShown: Bool { panel.isVisible }
    var keepOpen: Bool {
        get { panel.keepOpen }
        set { panel.keepOpen = newValue }
    }
    var rootView: NSView { panel.contentView! }

    override init() {
        panel = SearchPanel(
            contentRect: NSRect(x: 0, y: 0, width: Self.width, height: Self.collapsedHeight),
            styleMask: [.borderless, .nonactivatingPanel, .fullSizeContentView],
            backing: .buffered, defer: false)
        super.init()
        panel.level = .floating
        panel.isFloatingPanel = true
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        panel.hidesOnDeactivate = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.isReleasedWhenClosed = false
        panel.title = "MediaLake"

        let backdrop = NSVisualEffectView()
        backdrop.material = .hudWindow
        backdrop.blendingMode = .behindWindow
        backdrop.state = .active
        backdrop.wantsLayer = true
        backdrop.layer?.cornerRadius = 16
        backdrop.layer?.masksToBounds = true
        let host = NSHostingView(rootView: SearchView(model: model))
        host.translatesAutoresizingMaskIntoConstraints = false
        backdrop.addSubview(host)
        NSLayoutConstraint.activate([
            host.leadingAnchor.constraint(equalTo: backdrop.leadingAnchor),
            host.trailingAnchor.constraint(equalTo: backdrop.trailingAnchor),
            host.topAnchor.constraint(equalTo: backdrop.topAnchor),
            host.bottomAnchor.constraint(equalTo: backdrop.bottomAnchor),
        ])
        panel.contentView = backdrop

        panel.onResign = { [weak self] in
            guard let self, self.everShown else { return }
            self.hide()
        }
        model.onHide = { [weak self] in self?.hide() }
        model.objectWillChange
            .receive(on: DispatchQueue.main)
            .sink { [weak self] _ in self?.updateExpansion() }
            .store(in: &bag)
    }

    /// Shown centred on the screen under the mouse pointer.
    func show() {
        if model.setupVisible {
            model.onNeedSetup?()
            return
        }
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? NSScreen.main ?? NSScreen.screens[0]
        let wa = screen.visibleFrame
        let h = model.wantsExpanded ? Self.expandedHeight : Self.collapsedHeight
        let top = wa.maxY - wa.height * 0.16
        panel.setFrame(NSRect(x: wa.midX - Self.width / 2, y: top - h, width: Self.width, height: h), display: true)
        panel.makeKeyAndOrderFront(nil)
        everShown = true
        model.focusTick += 1
        Task {
            await model.checkSetup()
            // the model turned out to be missing: the setup window takes over
            if model.setupVisible, panel.isVisible {
                hide()
                model.onNeedSetup?()
            }
        }
    }

    func hide() { panel.orderOut(nil) }

    func toggle() { isShown ? hide() : show() }

    /// Grows the panel while typing and shrinks it back when the query is cleared; the top edge stays.
    private func updateExpansion() {
        guard panel.isVisible else { return }
        let target = model.wantsExpanded ? Self.expandedHeight : Self.collapsedHeight
        var f = panel.frame
        guard abs(f.height - target) > 0.5 else { return }
        f.origin.y += f.height - target
        f.size.height = target
        panel.setFrame(f, display: true, animate: true)
    }
}

// MARK: - SwiftUI

struct SearchView: View {
    @ObservedObject var model: SearchModel
    @FocusState private var queryFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("MediaLake").font(.system(size: 30, weight: .black))
                Spacer()
                Button { model.onOpenSettings?() } label: { Image(systemName: "gearshape").font(.system(size: 16)) }
                    .buttonStyle(.plain)
                    .help("Settings")
                    .padding(8)
            }
            TextField("Search files, images, audio…", text: $model.query)
                .textFieldStyle(.plain)
                .font(.system(size: 18))
                .padding(.horizontal, 12).padding(.vertical, 9)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.08)))
                .focused($queryFocused)
                .onChange(of: model.query) { _, _ in model.queryChanged() }
                .onKeyPress(.downArrow) { model.moveSelection(1); return .handled }
                .onKeyPress(.upArrow) { model.moveSelection(-1); return .handled }
                .onKeyPress(.escape) { model.onHide?(); return .handled }
                .onKeyPress(.return) {
                    model.openSelected(showInFinder: NSEvent.modifierFlags.contains(.command))
                    return .handled
                }
                .onChange(of: model.focusTick) { _, _ in queryFocused = true }
                .onAppear { queryFocused = true }

            // thin "searching" indicator right below the input box
            ZStack {
                if model.loading { ProgressView().progressViewStyle(.linear) }
            }
            .frame(height: 4)
            .padding(.horizontal, 2)

            ZStack {
                if model.resultsVisible {
                    ResultsList(model: model)
                } else {
                    Text(model.emptyText)
                        .opacity(0.55)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .frame(maxHeight: .infinity)

            VStack(alignment: .leading, spacing: 4) {
                if let f = model.indexFraction { ProgressView(value: f).progressViewStyle(.linear).frame(height: 4) }
                Text(model.statusText).font(.system(size: 12)).opacity(0.65).lineLimit(1).truncationMode(.tail)
            }
        }
        .padding(.horizontal, 24)
        .padding(.top, 18)
        .padding(.bottom, 14)
    }
}

struct ResultsList: View {
    @ObservedObject var model: SearchModel

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(Array(model.items.enumerated()), id: \.element.id) { index, item in
                        ResultRow(item: item, selected: index == model.selection, model: model)
                            .id(item.id)
                            .onTapGesture { model.open(item, showInFinder: false) }
                    }
                }
            }
            .onChange(of: model.selection) { _, i in
                if model.items.indices.contains(i) { proxy.scrollTo(model.items[i].id) }
            }
        }
    }
}

struct ResultRow: View {
    @ObservedObject var item: ResultItem
    let selected: Bool
    let model: SearchModel

    var body: some View {
        HStack(spacing: 12) {
            ZStack {
                Image(systemName: item.symbol).font(.system(size: 22)).opacity(0.6)
                if let t = item.thumb {
                    Image(nsImage: t).resizable().aspectRatio(contentMode: .fill)
                }
            }
            .frame(width: 44, height: 44)
            .clipShape(RoundedRectangle(cornerRadius: 6))

            VStack(alignment: .leading, spacing: 1) {
                Text(item.name).fontWeight(.semibold).lineLimit(1).truncationMode(.tail)
                Text(item.folder).font(.system(size: 12)).opacity(0.6).lineLimit(1).truncationMode(.middle)
                if !item.snippet.isEmpty {
                    Text(item.snippet).font(.system(size: 12)).opacity(0.75).lineLimit(2)
                }
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Text(item.modality)
                    .font(.system(size: 11, weight: .semibold)).foregroundColor(.white)
                    .padding(.horizontal, 7).padding(.vertical, 1)
                    .background(RoundedRectangle(cornerRadius: 4).fill(item.badgeColor))
                Text(item.scoreText).font(.system(size: 11)).opacity(0.6)
            }
            Button { model.open(item, showInFinder: true) } label: { Image(systemName: "folder").font(.system(size: 16)) }
                .buttonStyle(.plain)
                .help("Show in Finder (⌘Return)")
                .padding(8)
                .accessibilityLabel("Show in Finder")
        }
        .padding(.horizontal, 6).padding(.vertical, 6)
        .background(RoundedRectangle(cornerRadius: 8).fill(selected ? Color.accentColor.opacity(0.22) : Color.clear))
        .contentShape(Rectangle())
    }
}

// MARK: - First-run setup window

/// A normal window (so it can always be closed with the button, ⌘W or Esc). Closing it does not stop a
/// running download: the download lives in the core and the model, not in the window.
final class SetupWindow: NSWindow {
    override func cancelOperation(_ sender: Any?) { performClose(nil) }
}

@MainActor
final class SetupWindowController: NSObject {
    let window: SetupWindow
    var rootView: NSView { window.contentView! }

    init(model: SearchModel) {
        window = SetupWindow(
            contentRect: NSRect(x: 0, y: 0, width: 540, height: 10),
            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        window.title = "MediaLake"
        window.isReleasedWhenClosed = false
        let host = NSHostingView(rootView: SetupView(model: model))
        window.contentView = host
        window.setContentSize(host.fittingSize)
        window.center()
    }

    func show() {
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func close() { window.close() }
}

struct SetupView: View {
    @ObservedObject var model: SearchModel

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Welcome to MediaLake").font(.system(size: 26, weight: .bold))
            if model.setupDone {
                ready
            } else {
                download
            }
        }
        .padding(28)
        .frame(width: 540, alignment: .leading)
    }

    private var download: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("MediaLake understands your files with an AI model that runs on this computer. It is downloaded once and stays here. Pick a quality (you can change it later in Settings):")
                .opacity(0.8).fixedSize(horizontal: false, vertical: true)
            Picker("", selection: $model.setupQuality) {
                ForEach(model.qualities) { q in Text(q.label).tag(q.name) }
            }
            .labelsHidden()
            .frame(maxWidth: 460, alignment: .leading)
            .disabled(model.downloading)

            VStack(alignment: .leading, spacing: 6) {
                Text("Global shortcut").fontWeight(.semibold)
                HotkeyRecorder(text: $model.hotkeyText, error: $model.hotkeyError) { model.applyHotkey?($0) }
            }
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Start MediaLake when I sign in", isOn: Binding(get: { model.autostart }, set: { model.setAutostart($0) }))
                if !model.autostartError.isEmpty { Text(model.autostartError).foregroundColor(.red).font(.system(size: 12)) }
            }

            if let p = model.setupProgress { ProgressView(value: p).progressViewStyle(.linear) }
            if !model.setupInfo.isEmpty { Text(model.setupInfo).opacity(0.75).fixedSize(horizontal: false, vertical: true) }
            if model.downloading {
                Text("You can close this window: the download continues in the background.").opacity(0.6).font(.system(size: 12))
            }

            HStack {
                Spacer()
                Button(model.downloading ? "Hide" : "Close") { model.onCloseSetup?() }.keyboardShortcut(.cancelAction)
                Button("Download") { Task { await model.download() } }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.downloading || model.qualities.isEmpty)
            }
        }
    }

    private var ready: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("The model is ready. Press \(HotkeySpec.display(of: model.hotkeyText)) anywhere to search. Add the folders you want to search in Settings.")
                .opacity(0.8).fixedSize(horizontal: false, vertical: true)
            HStack {
                Spacer()
                Button("Close") { model.onCloseSetup?() }.keyboardShortcut(.cancelAction)
                Button("Add folders…") {
                    model.onCloseSetup?()
                    model.onOpenSettings?()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
            }
        }
    }
}
