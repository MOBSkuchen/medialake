import AppKit
import SwiftUI
@preconcurrency import UserNotifications

// MARK: - Notifications

/// System notifications for things that need a decision (a conflict in an "ask" folder) or attention (a
/// host that went away for good, a full disk). Clicking one opens Settings > Sync.
@MainActor
final class SyncNotifier: NSObject, UNUserNotificationCenterDelegate {
    var onOpen: (() -> Void)?

    /// `swift run` has no bundle, and UNUserNotificationCenter refuses to exist without one.
    private var available: Bool { Bundle.main.bundleURL.pathExtension == "app" }
    private var lastShown: [String: Date] = [:]

    func start() {
        guard available else { return }
        UNUserNotificationCenter.current().delegate = self
    }

    func notify(id: String, title: String, body: String) {
        guard available else { return }
        // the same message at most every ten minutes
        if let t = lastShown[id], Date().timeIntervalSince(t) < 600 { return }
        lastShown[id] = Date()
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            guard granted else { return }
            let c = UNMutableNotificationContent()
            c.title = title
            c.body = body
            center.add(UNNotificationRequest(identifier: id, content: c, trigger: nil))
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                            withCompletionHandler completionHandler: @escaping () -> Void) {
        Task { @MainActor in self.onOpen?() }
        completionHandler()
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                            withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }
}

// MARK: - First run: search, sync, or both

enum FirstRunChoice: String {
    case search, sync, both

    var searchEnabled: Bool { self != .sync }
    var syncEnabled: Bool { self != .search }
}

enum FirstRun {
    private static let key = "firstRunChoiceMade"

    static var needed: Bool { !UserDefaults.standard.bool(forKey: key) }
    static func markDone() { UserDefaults.standard.set(true, forKey: key) }

    /// Writes the choice into the core's settings.
    static func apply(_ c: FirstRunChoice) async {
        guard let core = AppState.core else { return }
        if var s = try? await core.callAsync("get_settings") {
            s["search_enabled"] = c.searchEnabled
            _ = try? await core.callAsync("set_settings", s)
        }
        _ = try? await core.callAsync("sync_enable", ["enabled": c.syncEnabled])
        markDone()
    }
}

@MainActor
final class FirstRunWindowController: NSObject, NSWindowDelegate {
    let window: NSWindow
    var onChoice: ((FirstRunChoice) -> Void)?

    override init() {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 360), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        window.title = "Welcome to MediaLake"
        window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: FirstRunView { [weak self] c in
            self?.window.close()
            self?.onChoice?(c)
        })
        window.center()
        window.delegate = self
    }

    func show() {
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }
}

private struct FirstRunView: View {
    let choose: (FirstRunChoice) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("What would you like to use MediaLake for?").font(.title).fontWeight(.semibold)
            Text("You can change this at any time in Settings.").opacity(0.7)
            HStack(spacing: 14) {
                card("magnifyingglass", "Search", "Find files by what is in them: text, pictures, sound and video. Needs a model of about 1 GB.", .search)
                card("arrow.triangle.2.circlepath", "Sync", "Keep folders in step between your computers. Files are loaded when you open them.", .sync)
                card("sparkles", "Both", "Search, and search inside your synced folders too.", .both)
            }
        }
        .padding(28)
        .frame(width: 640)
    }

    private func card(_ symbol: String, _ title: String, _ text: String, _ c: FirstRunChoice) -> some View {
        Button { choose(c) } label: {
            VStack(alignment: .leading, spacing: 10) {
                Image(systemName: symbol).font(.system(size: 28)).foregroundColor(.accentColor)
                Text(title).font(.title3).fontWeight(.semibold)
                Text(text).font(.system(size: 12)).opacity(0.75).fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .padding(14)
            .frame(width: 184, height: 190, alignment: .topLeading)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color.primary.opacity(0.06)))
            .overlay(RoundedRectangle(cornerRadius: 10).stroke(Color.primary.opacity(0.15)))
        }
        .buttonStyle(.plain)
    }
}
