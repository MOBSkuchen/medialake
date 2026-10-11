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
    private var statusItem: StatusItemController?
    private var hotkeyText = "Alt+Space"
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
        main.model.applyHotkey = { [weak self] in self?.applyHotkey($0) }
        AppState.core?.onEvent = { [weak self] ev in
            self?.main.model.handle(event: ev)
            self?.settings?.model.handle(event: ev)
        }

        if let c = AppState.core, let s = try? c.callObject("get_settings"), let h = s["hotkey"] as? String { hotkeyText = h }
        // a Windows config can carry Win+Space, which parses as Cmd+Space: Spotlight's
        if let who = HotkeySpec.parse(hotkeyText)?.reservedBy {
            logLine("hotkey \(hotkeyText) is used by macOS for \(who); falling back to Alt+Space")
            hotkeyText = "Alt+Space"
        }
        Hotkey.onPressed = { [weak self] in self?.toggle() }
        if let err = Hotkey.register(hotkeyText) { logLine("hotkey \(hotkeyText): \(err)") }
        main.model.hotkeyText = hotkeyText

        let item = StatusItemController(tooltip: "MediaLake  (\(HotkeySpec.display(of: hotkeyText)))")
        item.onOpen = { [weak self] in self?.toggle() }
        item.onSettings = { [weak self] in self?.showSettings() }
        item.onQuit = { [weak self] in self?.quit() }
        statusItem = item

        if let e = AppState.startupError { main.model.handle(event: ["type": "error", "message": e]) }

        if testMode {
            Task { await runTest(selfTest: selfTest, shotPath: shot) }
        } else if !argv.contains("--background") && !launchedAtLogin() && AppState.core != nil && isFirstRun() {
            main.show()
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        // a second launch (LSMultipleInstancesProhibited) lands here
        main.show()
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

    private func showSettings() {
        if settings == nil {
            let s = SettingsWindowController()
            s.model.applyHotkey = { [weak self] in self?.applyHotkey($0) }
            s.model.requestQuit = { [weak self] in self?.quit() }
            s.model.uninstall = { [weak self] in self?.uninstall() }
            s.onClose = { [weak self] in self?.settings = nil }
            settings = s
        }
        main.hide()
        settings?.show()
    }

    /// Registers the new hotkey; on success updates the tooltip and the first-run field.
    private func applyHotkey(_ text: String) -> String? {
        if let err = Hotkey.register(text) { return err }
        hotkeyText = text
        statusItem?.setTooltip("MediaLake  (\(HotkeySpec.display(of: text)))")
        main.model.hotkeyText = text
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
        performUninstall(deleteData: r == .alertFirstButtonReturn)
    }

    private func performUninstall(deleteData: Bool) {
        if let c = AppState.core { _ = try? c.call("forget_remote") }  // the Keychain item
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
