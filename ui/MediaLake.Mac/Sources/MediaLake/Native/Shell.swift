import AppKit
import Carbon.HIToolbox
import ServiceManagement

// MARK: - Hotkey

/// A parsed hotkey string such as `Alt+Space`. Tokens: Ctrl, Alt (Option), Shift, Cmd. `Win` and `Meta`
/// parse as Cmd so a config.toml copied from Windows still loads.
struct HotkeySpec: Equatable {
    var modifiers: UInt32  // Carbon modifier mask
    var keyCode: UInt32

    static let keyNames: [String: UInt32] = {
        var m: [String: UInt32] = [
            "A": 0, "S": 1, "D": 2, "F": 3, "H": 4, "G": 5, "Z": 6, "X": 7, "C": 8, "V": 9, "B": 11, "Q": 12, "W": 13, "E": 14,
            "R": 15, "Y": 16, "T": 17, "O": 31, "U": 32, "I": 34, "P": 35, "L": 37, "J": 38, "K": 40, "N": 45, "M": 46,
            "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "9": 25, "7": 26, "8": 28, "0": 29,
            "Enter": 36, "Tab": 48, "Space": 49, "Back": 51, "Escape": 53,
            "Left": 123, "Right": 124, "Down": 125, "Up": 126,
            "Minus": 27, "Equal": 24, "LeftBracket": 33, "RightBracket": 30, "Quote": 39, "Semicolon": 41, "Backslash": 42,
            "Comma": 43, "Slash": 44, "Period": 47, "Grave": 50,
            "F1": 122, "F2": 120, "F3": 99, "F4": 118, "F5": 96, "F6": 97, "F7": 98, "F8": 100, "F9": 101, "F10": 109,
            "F11": 103, "F12": 111,
        ]
        m["Return"] = 36
        return m
    }()

    static let namesByCode: [UInt32: String] = {
        var r: [UInt32: String] = [:]
        for (k, v) in keyNames where k != "Return" { r[v] = k }
        return r
    }()

    static func parse(_ text: String) -> HotkeySpec? {
        var mods: UInt32 = 0
        var key: UInt32?
        for raw in text.split(separator: "+").map({ $0.trimmingCharacters(in: .whitespaces) }) where !raw.isEmpty {
            switch raw.lowercased() {
            case "ctrl", "control": mods |= UInt32(controlKey)
            case "alt", "option", "opt": mods |= UInt32(optionKey)
            case "shift": mods |= UInt32(shiftKey)
            case "cmd", "command", "win", "meta": mods |= UInt32(cmdKey)
            default:
                guard key == nil else { return nil }
                let name = raw.count == 1 ? raw.uppercased() : (raw.prefix(1).uppercased() + raw.dropFirst())
                var code = keyNames[name]
                if code == nil, name.hasPrefix("Number"), name.count == 7 { code = keyNames[String(name.suffix(1))] }
                guard let code else { return nil }
                key = code
            }
        }
        guard let key, mods != 0 else { return nil }
        return HotkeySpec(modifiers: mods, keyCode: key)
    }

    /// The config.toml spelling.
    var text: String {
        var parts: [String] = []
        if modifiers & UInt32(controlKey) != 0 { parts.append("Ctrl") }
        if modifiers & UInt32(optionKey) != 0 { parts.append("Alt") }
        if modifiers & UInt32(shiftKey) != 0 { parts.append("Shift") }
        if modifiers & UInt32(cmdKey) != 0 { parts.append("Cmd") }
        parts.append(HotkeySpec.namesByCode[keyCode] ?? "Key\(keyCode)")
        return parts.joined(separator: "+")
    }

    /// Display spelling with symbols, e.g. `⌥Space`.
    var display: String { HotkeySpec.display(of: text) }

    static func display(of text: String) -> String {
        guard let s = parse(text) else { return text }
        var out = ""
        if s.modifiers & UInt32(controlKey) != 0 { out += "⌃" }
        if s.modifiers & UInt32(optionKey) != 0 { out += "⌥" }
        if s.modifiers & UInt32(shiftKey) != 0 { out += "⇧" }
        if s.modifiers & UInt32(cmdKey) != 0 { out += "⌘" }
        return out + (namesByCode[s.keyCode] ?? "?")
    }

    /// Combinations that macOS or common launchers own. Registering one of these succeeds, but the
    /// system shortcut wins or both fire, so they are rejected up front.
    static let reserved: [String: String] = [
        "Cmd+Space": "Spotlight", "Ctrl+Space": "switching the input source", "Ctrl+Cmd+Space": "the emoji picker",
        "Cmd+Alt+Space": "the Finder search", "Cmd+Tab": "the app switcher", "Cmd+Shift+Tab": "the app switcher",
        "Cmd+Q": "quitting apps", "Cmd+W": "closing windows", "Cmd+H": "hiding apps", "Cmd+M": "minimising windows",
        "Cmd+Shift+3": "screenshots", "Cmd+Shift+4": "screenshots", "Cmd+Shift+5": "screenshots",
        "Ctrl+Cmd+Q": "locking the screen", "Cmd+Alt+Escape": "Force Quit",
    ]

    var reservedBy: String? {
        let wanted = HotkeySpec.reserved.first { HotkeySpec.parse($0.key) == self }
        return wanted?.value
    }

    static func from(event: NSEvent) -> HotkeySpec? {
        var mods: UInt32 = 0
        let f = event.modifierFlags
        if f.contains(.control) { mods |= UInt32(controlKey) }
        if f.contains(.option) { mods |= UInt32(optionKey) }
        if f.contains(.shift) { mods |= UInt32(shiftKey) }
        if f.contains(.command) { mods |= UInt32(cmdKey) }
        guard namesByCode[UInt32(event.keyCode)] != nil else { return nil }
        return HotkeySpec(modifiers: mods, keyCode: UInt32(event.keyCode))
    }
}

/// Carbon `RegisterEventHotKey`: needs neither Accessibility nor Input Monitoring permission.
@MainActor
enum Hotkey {
    static var onPressed: (() -> Void)?
    private static var ref: EventHotKeyRef?
    private static var handlerInstalled = false

    /// Registers the hotkey (the previous one is released first). Returns an error text or nil.
    static func register(_ text: String) -> String? {
        guard let spec = HotkeySpec.parse(text) else { return "Use at least one modifier and one key, e.g. ⌥Space." }
        installHandler()
        unregister()
        var newRef: EventHotKeyRef?
        let id = EventHotKeyID(signature: OSType(0x4D4C4B59) /* 'MLKY' */, id: 1)
        let status = RegisterEventHotKey(spec.keyCode, spec.modifiers, id, GetApplicationEventTarget(), 0, &newRef)
        guard status == noErr else { return "This shortcut is already used by another application." }
        ref = newRef
        return nil
    }

    static func unregister() {
        if let ref { UnregisterEventHotKey(ref) }
        ref = nil
    }

    private static func installHandler() {
        guard !handlerInstalled else { return }
        handlerInstalled = true
        var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(GetApplicationEventTarget(), { _, _, _ -> OSStatus in
            DispatchQueue.main.async { MainActor.assumeIsolated { Hotkey.onPressed?() } }
            return noErr
        }, 1, &spec, nil, nil)
    }
}

// MARK: - Status item

@MainActor
final class StatusItemController: NSObject {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let menu = NSMenu()
    var onOpen: (() -> Void)?
    var onSettings: (() -> Void)?
    var onQuit: (() -> Void)?
    var onPauseAll: ((Bool) -> Void)?

    // what the sync part of the menu shows
    private var syncLines: [String] = []
    private var allPaused = false
    private var hasSyncs = false

    init(tooltip: String) {
        super.init()
        if let button = item.button {
            let img = NSImage(systemSymbolName: "water.waves", accessibilityDescription: "MediaLake")
                ?? NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: "MediaLake")
            img?.isTemplate = true
            button.image = img
            button.imagePosition = .imageLeading
            button.toolTip = tooltip
            button.target = self
            button.action = #selector(clicked)
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }
        rebuildMenu()
    }

    func setTooltip(_ s: String) { item.button?.toolTip = s }

    /// One line per sync, "Pause all", and a badge next to the icon: orange dot = needs a decision or
    /// an error, blue arrows = transferring.
    func updateSync(lines: [String], allPaused: Bool, attention: Bool, busy: Bool) {
        syncLines = lines
        hasSyncs = !lines.isEmpty
        self.allPaused = allPaused
        rebuildMenu()
        guard let button = item.button else { return }
        let (glyph, color): (String, NSColor) = attention ? (" ●", .systemOrange) : busy ? (" ↻", .systemBlue) : ("", .labelColor)
        button.attributedTitle = NSAttributedString(string: glyph, attributes: [.foregroundColor: color, .font: NSFont.systemFont(ofSize: 11)])
    }

    private func rebuildMenu() {
        menu.removeAllItems()
        for (title, sel) in [("Open MediaLake", #selector(openAction)), ("Settings…", #selector(settingsAction))] {
            let mi = NSMenuItem(title: title, action: sel, keyEquivalent: "")
            mi.target = self
            menu.addItem(mi)
        }
        if hasSyncs {
            menu.addItem(.separator())
            for l in syncLines {
                let mi = NSMenuItem(title: l, action: nil, keyEquivalent: "")
                mi.isEnabled = false
                menu.addItem(mi)
            }
            let pause = NSMenuItem(title: allPaused ? "Resume all syncing" : "Pause all syncing", action: #selector(pauseAction), keyEquivalent: "")
            pause.target = self
            menu.addItem(pause)
        }
        menu.addItem(.separator())
        let quit = NSMenuItem(title: "Quit MediaLake", action: #selector(quitAction), keyEquivalent: "")
        quit.target = self
        menu.addItem(quit)
    }

    @objc private func clicked() {
        if NSApp.currentEvent?.type == .rightMouseUp {
            item.menu = menu
            item.button?.performClick(nil)
            item.menu = nil
        } else {
            onOpen?()
        }
    }

    @objc private func openAction() { onOpen?() }
    @objc private func settingsAction() { onSettings?() }
    @objc private func quitAction() { onQuit?() }
    @objc private func pauseAction() { onPauseAll?(!allPaused) }
}

// MARK: - File actions, login item, folder access

enum ShellActions {
    static func open(_ path: String) { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }

    static func showInFinder(_ path: String) {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    static func openFilesAndFoldersSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders") {
            NSWorkspace.shared.open(url)
        }
    }
}

enum LoginItem {
    static var isEnabled: Bool { SMAppService.mainApp.status == .enabled }

    /// Returns an error text or nil.
    @discardableResult
    static func set(_ on: Bool) -> String? {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            return nil
        } catch {
            return error.localizedDescription
        }
    }
}

enum FolderAccess {
    /// True when the folder exists but this app is not allowed to read it (a denied privacy prompt).
    static func isDenied(_ path: String) -> Bool {
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir), isDir.boolValue else { return false }
        do {
            _ = try FileManager.default.contentsOfDirectory(atPath: path)
            return false
        } catch let e as NSError {
            if e.domain == NSCocoaErrorDomain, e.code == NSFileReadNoPermissionError { return true }
            if let u = e.userInfo[NSUnderlyingErrorKey] as? NSError, u.domain == NSPOSIXErrorDomain, u.code == Int(EPERM) || u.code == Int(EACCES) {
                return true
            }
            return false
        }
    }
}

// MARK: - Paths and logging

enum AppPaths {
    static var dataDir: URL {
        if let h = ProcessInfo.processInfo.environment["MEDIALAKE_HOME"], !h.isEmpty { return URL(fileURLWithPath: h) }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/MediaLake")
    }
    static var logsDir: URL { dataDir.appendingPathComponent("logs") }
}

func logLine(_ s: String) {
    let dir = AppPaths.logsDir
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let line = "\(ISO8601DateFormatter().string(from: Date())) \(s)\n"
    let url = dir.appendingPathComponent("ui.log")
    if let h = try? FileHandle(forWritingTo: url) {
        h.seekToEndOfFile()
        h.write(Data(line.utf8))
        try? h.close()
    } else {
        try? line.write(to: url, atomically: false, encoding: .utf8)
    }
}
