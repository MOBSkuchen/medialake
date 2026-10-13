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
