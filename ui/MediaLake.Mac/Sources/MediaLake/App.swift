import AppKit
import Carbon.HIToolbox
import SwiftUI

/// Process-wide state (the C# `App.Core` static).
@MainActor
enum AppState {
    nonisolated(unsafe) static var core: CoreClient?
    static var startupError: String?
}

@main
struct MediaLakeMain {
    @MainActor
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory)  // no Dock icon, no app menu (LSUIElement)
        app.run()
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var main: MainWindowController!
    private var settings: SettingsWindowController?
    private var welcome: WelcomeWindowController?
    private var statusItem: StatusItemController?
    private let sync = SyncModel()
    private let notifier = SyncNotifier()
    /// Empty = no global shortcut (nobody has to set one).
    private var hotkeyText = ""
    private var testMode = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        let argv = CommandLine.arguments
        let selfTest = argv.contains("--selftest")
        let shotIdx = argv.firstIndex(of: "--screenshot")
        let shot = shotIdx.flatMap { $0 + 1 < argv.count ? argv[$0 + 1] : nil }
        testMode = selfTest || shot != nil
        installMainMenu()

        do {
            let exe = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/medialake-server").path
            var serverExe = exe
            if !FileManager.default.fileExists(atPath: exe) {
                // `swift run` / a plain build folder: the server sits next to the executable
                serverExe = URL(fileURLWithPath: argv[0]).deletingLastPathComponent().appendingPathComponent("medialake-server").path
            }
            let home = ProcessInfo.processInfo.environment["MEDIALAKE_HOME"]
            AppState.core = try CoreClient.create(configDir: home, options: ["server_exe": serverExe])
        } catch {
            AppState.startupError = error.localizedDescription
            logLine("core init failed: \(error)")
        }

        main = MainWindowController()
        main.model.onOpenSettings = { [weak self] in self?.showSettings() }
        main.model.onOpenWelcome = { [weak self] in self?.showWelcome(page: .search) }
        AppState.core?.onEvent = { [weak self] ev in
            self?.main.model.handle(event: ev)
            self?.settings?.model.handle(event: ev)
            self?.welcome?.model.handle(event: ev)
            self?.sync.handle(event: ev)
            self?.notifyAboutSync(ev)
        }
        notifier.onOpen = { [weak self] in self?.showSettings(page: .sync) }
        notifier.start()
        sync.onSummary = { [weak self] folders, conflicts in self?.updateSyncSummary(folders, conflicts) }

        if let c = AppState.core, let s = try? c.callObject("get_settings") {
            hotkeyText = s["hotkey"] as? String ?? ""
        }
        // a Windows config can carry Win+Space, which parses as Cmd+Space: Spotlight's
        if !hotkeyText.isEmpty, let who = HotkeySpec.parse(hotkeyText)?.reservedBy {
            logLine("hotkey \(hotkeyText) is used by macOS for \(who); falling back to Alt+Space")
            hotkeyText = "Alt+Space"
        }
        Hotkey.onPressed = { [weak self] in self?.toggle() }
        if !hotkeyText.isEmpty, let err = Hotkey.register(hotkeyText) { logLine("hotkey \(hotkeyText): \(err)") }

        let item = StatusItemController(tooltip: Self.tooltip(for: hotkeyText))
        item.onOpen = { [weak self] in self?.toggle() }
        item.onSettings = { [weak self] in self?.showSettings() }
        item.onQuit = { [weak self] in self?.quit() }
        item.onPauseAll = { [weak self] paused in
            guard let self else { return }
            Task { await self.sync.setAllPaused(paused) }
        }
        statusItem = item
        Task { await sync.refresh() }

        if let e = AppState.startupError { main.model.handle(event: ["type": "error", "message": e]) }

        let background = argv.contains("--background") || launchedAtLogin()
        if testMode {
            Task { await runTest(selfTest: selfTest, shotPath: shot) }
        } else if AppState.core != nil {
            Task {
                await main.model.refreshSearchState()
                // a fresh install (no config yet) and not started in the background: welcome the user.
                // A background start leaves it for the next time the app is opened.
                let done = (try? AppState.core?.callObject("get_settings"))?["welcome_done"] as? Bool ?? true
                if !done {
                    if !background { showWelcome(page: .search) }
                    return
                }
                await continueStartup(background: background)
            }
        }
    }

    /// Start-up of an app that has been set up: open nothing unless there is nothing to see yet.
    private func continueStartup(background: Bool) async {
        await sync.refresh()
        if background { return }
        if main.model.searchState == "ready" {
            if isFirstRun() { main.show() }
        } else if sync.folders.isEmpty {
            main.show()  // the search window explains what is missing
        } else {
            showSettings(page: .sync)
        }
    }

    /// The welcome window (shortcut and model, then sync). `page` is where it opens.
    private func showWelcome(page: WelcomeModel.Page) {
        if welcome == nil {
            let w = WelcomeWindowController(sync: sync)
            w.model.applyHotkey = { [weak self] in self?.applyHotkey($0) }
            w.model.onSearchChanged = { [weak self] in self?.searchChanged() }
            w.onClosed = { [weak self] in
                self?.welcome = nil
                self?.searchChanged()
            }
            welcome = w
        }
        main.hide()
        logLine("welcome window shown (\(page == .search ? "search" : "sync") page)")
        welcome?.show(page: page)
    }

    /// Search was set up or switched off (by the welcome window): everything that shows it re-reads it.
    private func searchChanged() {
        Task {
            await main.model.refreshSearchState()
            // an open Settings window holds an older copy of the shortcut, model and search switch
            await settings?.model.load()
        }
    }

    static func tooltip(for hotkey: String) -> String {
        hotkey.isEmpty ? "MediaLake" : "MediaLake  (\(HotkeySpec.display(of: hotkey)))"
    }

    // MARK: sync status

    private func updateSyncSummary(_ folders: [SyncFolder], _ conflicts: Int) {
        let lines = folders.map { "\($0.name): \($0.stateText)" }
        statusItem?.updateSync(
            lines: lines,
            allPaused: !folders.isEmpty && folders.allSatisfy { $0.paused },
            attention: conflicts > 0 || folders.contains { $0.needsAttention },
            busy: folders.contains { $0.state == "syncing" })
    }

    private func notifyAboutSync(_ ev: [String: Any]) {
        switch ev["type"] as? String {
        case "sync_conflict":
            let name = ev["name"] as? String ?? "A synced folder"
            let path = (ev["path"] as? String ?? "") as NSString
            notifier.notify(id: "conflict-\(name)-\(path)", title: "Conflict in \(name)",
                            body: "\(path.lastPathComponent) was changed here and on another computer. Choose which version to keep.")
        case "sync_state":
            let state = ev["state"] as? String ?? ""
            let name = ev["name"] as? String ?? "A synced folder"
            if state == "denied" {
                notifier.notify(id: "denied-\(name)", title: "\(name) is no longer shared with you",
                                body: "The computer that shares it withdrew access. Your downloaded files stay where they are.")
            } else if state == "disk_full" {
                notifier.notify(id: "full-\(name)", title: "Your disk is full", body: "\(name) cannot be synced until some space is freed.")
            } else if state == "held" {
                notifier.notify(id: "held-\(name)", title: "Deletions in \(name) are waiting",
                                body: "Many files were removed here. Open MediaLake to confirm that they should be deleted everywhere.")
            }
        default:
            break
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        // a second launch (LSMultipleInstancesProhibited) lands here
        if let w = welcome {
            w.window.makeKeyAndOrderFront(nil)
        } else {
            main.show()
        }
        return false
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func applicationWillTerminate(_ notification: Notification) {
        Hotkey.unregister()
        AppState.core?.dispose()
    }

    // MARK: helpers

    private func launchedAtLogin() -> Bool {
        let ev = NSAppleEventManager.shared().currentAppleEvent
        return ev?.eventID == AEEventID(kAEOpenApplication)
            && ev?.paramDescriptor(forKeyword: AEKeyword(keyAEPropData))?.enumCodeValue == OSType(keyAELaunchedAsLogInItem)
    }

    private func isFirstRun() -> Bool {
        guard let s = try? AppState.core?.callObject("get_settings") else { return false }
        return ((s["dirs"] as? [Any])?.count ?? 0) == 0
    }

    private func toggle() { main.toggle() }

    private func showSettings(page: SettingsModel.Page? = nil) {
        if settings == nil {
            let s = SettingsWindowController(sync: sync)
            s.model.applyHotkey = { [weak self] in self?.applyHotkey($0) }
            s.model.openSearchSetup = { [weak self] in self?.showWelcome(page: .search) }
            s.model.requestQuit = { [weak self] in self?.quit() }
            s.model.uninstall = { [weak self] in self?.uninstall() }
            s.onClose = { [weak self] in self?.settings = nil }
            settings = s
        }
        main.hide()
        settings?.show()
        if let page { settings?.model.page = page }
    }

    /// Registers the new hotkey (empty = release it); on success updates the tooltip.
    private func applyHotkey(_ text: String) -> String? {
        if text.isEmpty {
            Hotkey.unregister()
        } else if let err = Hotkey.register(text) {
            return err
        }
        hotkeyText = text
        statusItem?.setTooltip(Self.tooltip(for: text))
        return nil
    }

    private func quit() {
        statusItem = nil
        Hotkey.unregister()
        AppState.core?.dispose()
        AppState.core = nil
        exit(0)
    }

    /// Standard Cut/Copy/Paste key equivalents need a main menu, even though an accessory app never shows it.
    private func installMainMenu() {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        menu.addItem(appItem)
        let appMenu = NSMenu(title: "MediaLake")
        appMenu.addItem(withTitle: "Quit MediaLake", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu

        let editItem = NSMenuItem()
        menu.addItem(editItem)
        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = edit

        let windowItem = NSMenuItem()
        menu.addItem(windowItem)
        let window = NSMenu(title: "Window")
        window.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        windowItem.submenu = window
        NSApp.mainMenu = menu
    }

    // MARK: uninstall (docs/plan2.md §5.3)

    private func uninstall() {
        let alert = NSAlert()
        alert.messageText = "Uninstall MediaLake?"
        alert.informativeText = "MediaLake quits and moves itself to the Trash. Do you also want to delete the index, settings and downloaded models?"
        alert.addButton(withTitle: "Uninstall and delete my data")
        alert.addButton(withTitle: "Uninstall, keep my data")
        alert.addButton(withTitle: "Cancel")
        let r = alert.runModal()
        guard r == .alertFirstButtonReturn || r == .alertSecondButtonReturn else { return }
        var keepSyncFiles = true
        if !sync.folders.isEmpty {
            let a = NSAlert()
            a.messageText = "Synced folders"
            a.informativeText = "MediaLake unmounts \(sync.folders.map { $0.name }.joined(separator: ", ")). " +
                "Do you want to keep the files you have downloaded from them? They are copied into the folders under ~/MediaLake."
            a.addButton(withTitle: "Keep downloaded files")
            a.addButton(withTitle: "Delete them")
            a.addButton(withTitle: "Cancel")
            let r2 = a.runModal()
            guard r2 == .alertFirstButtonReturn || r2 == .alertSecondButtonReturn else { return }
            keepSyncFiles = r2 == .alertFirstButtonReturn
        }
        performUninstall(deleteData: r == .alertFirstButtonReturn, keepSyncFiles: keepSyncFiles)
    }

    private func performUninstall(deleteData: Bool, keepSyncFiles: Bool = true) {
        if let c = AppState.core {
            // leave the syncs first: that unmounts the volumes (and copies what was downloaded if asked),
            // then the paired computers' secrets go with the other credentials
            if let st = try? c.callObject("sync_status"), let syncs = st["syncs"] as? [[String: Any]] {
                for f in syncs {
                    if let id = f["sync_id"] as? String {
                        if (try? c.call("sync_leave", ["sync_id": id, "keep_files": keepSyncFiles])) == nil { logLine("could not leave sync \(id)") }
                    }
                }
                if let h = try? c.callObject("sync_hosts"), let hosts = h["hosts"] as? [[String: Any]] {
                    for x in hosts { if let id = x["id"] as? String { _ = try? c.call("sync_forget_host", ["id": id]) } }
                }
            }
            _ = try? c.call("forget_remote")  // the Keychain item
        }
        _ = LoginItem.set(false)
        statusItem = nil
        Hotkey.unregister()
        AppState.core?.dispose()
        AppState.core = nil
        if deleteData { try? FileManager.default.removeItem(at: AppPaths.dataDir) }

        // a detached helper waits for this process to exit, then moves the bundle to the Trash
        let app = Bundle.main.bundleURL.path
        let script = """
        while kill -0 \(getpid()) 2>/dev/null; do sleep 0.2; done
        mv "$1" "$HOME/.Trash/MediaLake-$(date +%s).app" 2>/dev/null || \
          osascript -e "tell application \\"Finder\\" to delete POSIX file \\"$1\\"" >/dev/null 2>&1
        """
        if Bundle.main.bundleURL.pathExtension == "app" {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/sh")
            p.arguments = ["-c", script, "sh", app]
            p.standardInput = FileHandle.nullDevice
            p.standardOutput = FileHandle.nullDevice
            p.standardError = FileHandle.nullDevice
            try? p.run()
        }
        exit(0)
    }

    // MARK: automated checks (--selftest / --screenshot)

    private func runTest(selfTest: Bool, shotPath: String?) async {
        var log: [String] = []
        main.keepOpen = true
        let env = ProcessInfo.processInfo.environment
        main.show()
        try? await Task.sleep(nanoseconds: 800_000_000)
        let query = env["MEDIALAKE_TEST_QUERY"] ?? "a cat"
        if let core = AppState.core {
            // wait for the first index run so there is something to find
            let deadline = Date().addingTimeInterval(env["MEDIALAKE_TEST_NOWAIT"] != nil ? 0 : 240)
            while Date() < deadline {
                if let st = try? await core.callAsync("status"),
                   st["busy"] as? Bool == false, ((st["files"] as? NSNumber)?.uint64Value ?? 0) > 0 { break }
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
        }
        // MEDIALAKE_TEST_SYNC_DEMO=<mount point>: this app hosts "Docs" (config.toml) and joins it itself,
        // through the mount, so the Sync page has something real to show
        if let point = env["MEDIALAKE_TEST_SYNC_DEMO"], let core = AppState.core {
            do {
                let inv = try await core.callAsync("sync_invite", ["grants": [["sync": "Docs", "access": "rw"]]])
                let paired = try await core.callAsync("sync_pair", ["code": inv["code"] as? String ?? ""])
                let joined = try await core.callAsync("sync_join", [
                    "host": paired["host_id"] as? String ?? "", "sync_id": "Docs", "path": point, "frontend": "auto", "conflict": "ask",
                ])
                log.append("joined=\(joined)")
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                if let list = try? await core.callAsync("sync_browse", ["sync_id": "Docs", "path": ""]) { log.append("browse=\(list)") }
                // reading through the mount is what hydrates
                if let t = try? String(contentsOfFile: point + "/hello.txt", encoding: .utf8) { log.append("read hello.txt=\(t)") }
                try? "made on this Mac".write(toFile: point + "/from-mac.txt", atomically: true, encoding: .utf8)
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                if let st = try? await core.callAsync("sync_status") { log.append("sync_status=\(st)") }
                if let c = try? await core.callAsync("sync_conflicts") { log.append("sync_conflicts=\(c)") }
            } catch {
                log.append("sync demo failed: \(error)")
            }
            await sync.refresh()
        }
        main.model.query = query
        // the server starts on the first search, which can take a while: poll until results show up
        let waitMs = env["MEDIALAKE_TEST_WAIT"].flatMap { UInt64($0) } ?? 20000
        let until = Date().addingTimeInterval(Double(waitMs) / 1000)
        while Date() < until && main.model.items.isEmpty { try? await Task.sleep(nanoseconds: 250_000_000) }
        try? await Task.sleep(nanoseconds: 500_000_000)
        log.append("items=\(main.model.items.count)")
        for it in main.model.items { log.append("  \(it.scoreText) \(it.modality) \(it.path)") }
        if let st = try? AppState.core?.callObject("status") { log.append("status=\(st)") }
        // config.toml hotkeys copied from Windows must still parse
        for t in ["Ctrl+Alt+Space", "Win+Space", "Alt+Space", "Cmd+Shift+K", "Space"] {
            log.append("hotkey \(t) -> \(HotkeySpec.parse(t)?.display ?? "invalid")")
        }
        if let shotPath { await saveScreenshot(of: main.rootView, to: shotPath) }
        // MEDIALAKE_TEST_WELCOME=<folder>: renders the welcome window's pages there
        if let dir = env["MEDIALAKE_TEST_WELCOME"] {
            showWelcome(page: .search)
            welcome?.window.appearance = NSAppearance(named: .aqua)
            try? await Task.sleep(nanoseconds: 1_200_000_000)
            if let v = welcome?.rootView { await saveScreenshot(of: v, to: dir + "/welcome-search.png") }
            welcome?.model.previewDownload(0.42)
            try? await Task.sleep(nanoseconds: 400_000_000)
            if let v = welcome?.rootView { await saveScreenshot(of: v, to: dir + "/welcome-downloading.png") }
            welcome?.model.page = .sync
            try? await Task.sleep(nanoseconds: 400_000_000)
            if let v = welcome?.rootView { await saveScreenshot(of: v, to: dir + "/welcome-sync.png") }
            welcome?.window.close()
        }
        // MEDIALAKE_TEST_WELCOME_ACTION=skip|continue: presses that button on page 1 (a shortcut is staged
        // first) and logs what ended up in the settings
        if let action = env["MEDIALAKE_TEST_WELCOME_ACTION"] {
            showWelcome(page: .search)
            try? await Task.sleep(nanoseconds: 800_000_000)
            if let m = welcome?.model {
                await m.load()
                m.shortcut = "Ctrl+Alt+K"
                if action == "skip" { await m.skipSearch() } else { await m.download() }
                // the download path ends with search switched on; give the follow-up calls a moment
                let until = Date().addingTimeInterval(10)
                while m.downloading && Date() < until { try? await Task.sleep(nanoseconds: 200_000_000) }
                await m.finish()
            }
            welcome?.window.close()
            try? await Task.sleep(nanoseconds: 500_000_000)
            if let c = AppState.core, let s = try? await c.callAsync("get_settings"), let st = try? await c.callAsync("status") {
                log.append("welcome \(action): hotkey=\(s["hotkey"] ?? "?") search_enabled=\(s["search_enabled"] ?? "?") " +
                           "welcome_done=\(s["welcome_done"] ?? "?") model=\(s["model"] ?? "?") quality=\(s["quality"] ?? "?") " +
                           "status.search=\(st["search"] ?? "?") model_ready=\(st["model_ready"] ?? "?")")
            }
            await main.model.refreshSearchState()
            log.append("search window state: \(main.model.searchState)")
        }
        if let spec = env["MEDIALAKE_TEST_SETTINGS"] {
            let parts = spec.split(separator: "|", maxSplits: 1).map(String.init)
            if parts.count == 2, let page = Int(parts[0]), let p = SettingsModel.Page(rawValue: page) {
                showSettings()
                try? await Task.sleep(nanoseconds: 1_500_000_000)
                settings?.model.page = p
                // cacheDisplay leaves out the window background, which turns dark-mode text invisible
                settings?.window.appearance = NSAppearance(named: .aqua)
                try? await Task.sleep(nanoseconds: 1_500_000_000)
                if let v = settings?.rootView { await saveScreenshot(of: v, to: parts[1]) }
            }
        }
        // the bundle must stay untouched (a signed bundle breaks when written into), so this goes to the logs folder
        try? FileManager.default.createDirectory(at: AppPaths.logsDir, withIntermediateDirectories: true)
        try? log.joined(separator: "\n").write(to: AppPaths.logsDir.appendingPathComponent("selftest.log"), atomically: true, encoding: .utf8)
        // MEDIALAKE_TEST_UNINSTALL=keep|delete runs the uninstall steps without the confirmation dialog
        if let u = env["MEDIALAKE_TEST_UNINSTALL"] { performUninstall(deleteData: u == "delete") }
        quit()
    }

    private func saveScreenshot(of view: NSView, to path: String) async {
        try? await Task.sleep(nanoseconds: 300_000_000)
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
        // cacheDisplay leaves out the window background; without it dark-mode text lands on white
        let image = NSImage(size: view.bounds.size)
        image.lockFocus()
        view.effectiveAppearance.performAsCurrentDrawingAppearance {
            NSColor.windowBackgroundColor.setFill()
            NSRect(origin: .zero, size: view.bounds.size).fill()
        }
        rep.draw(in: NSRect(origin: .zero, size: view.bounds.size))
        image.unlockFocus()
        guard let tiff = image.tiffRepresentation, let out = NSBitmapImageRep(data: tiff),
              let png = out.representation(using: .png, properties: [:]) else { return }
        try? png.write(to: URL(fileURLWithPath: path))
    }
}
