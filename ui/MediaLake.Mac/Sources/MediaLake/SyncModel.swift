import AppKit
import SwiftUI

// MARK: - Rows

private extension Dictionary where Key == String, Value == Any {
    func str(_ k: String) -> String { self[k] as? String ?? "" }
    func int(_ k: String) -> Int64 { (self[k] as? NSNumber)?.int64Value ?? 0 }
    func flag(_ k: String) -> Bool { self[k] as? Bool ?? false }
}

func formatBytes(_ b: Int64) -> String {
    switch b {
    case ..<1024: return "\(b) B"
    case ..<(1 << 20): return String(format: "%.0f KB", Double(b) / 1024)
    case ..<(1 << 30): return String(format: "%.1f MB", Double(b) / 1_048_576)
    default: return String(format: "%.1f GB", Double(b) / 1_073_741_824)
    }
}

/// A sync this Mac has joined (one entry of `sync_status`).
struct SyncFolder: Identifiable, Equatable {
    var id: String
    var name: String
    var path: String
    var hostName: String
    var access: String
    var conflict: String
    var frontend: String
    var state: String
    var detail: String
    var notice: String
    var paused: Bool
    var lastSync: Date?
    var cachedBytes: Int64
    var cacheLimitBytes: Int64
    var unsynced: Int
    var conflicts: Int
    var heldDeletes: Int?

    init(json j: [String: Any]) {
        id = j.str("sync_id")
        name = j.str("name")
        path = j.str("path")
        hostName = j.str("host_name")
        access = j.str("access")
        conflict = j.str("conflict_policy")
        frontend = j.str("frontend")
        state = j.str("state")
        detail = j.str("detail")
        notice = j.str("notice")
        paused = j.flag("paused")
        lastSync = (j["last_sync"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue) }
        cachedBytes = j.int("cached_bytes")
        cacheLimitBytes = j.int("cache_limit_bytes")
        unsynced = Int(j.int("unsynced_files"))
        conflicts = Int(j.int("pending_conflicts"))
        heldDeletes = (j["held_deletes"] as? NSNumber).map { $0.intValue }
    }

    var isMount: Bool { frontend == "mount" }
    var readOnly: Bool { access != "rw" }

    /// One line for the status item and the folder card.
    var stateText: String {
        switch state {
        case "idle": return unsynced > 0 ? "\(unsynced) change\(unsynced == 1 ? "" : "s") not uploaded yet" : "Up to date"
        case "syncing": return "Syncing…"
        case "starting": return "Starting…"
        case "paused": return "Paused"
        case "conflict": return "\(max(conflicts, 1)) conflict\(conflicts == 1 ? "" : "s") need a decision"
        case "held": return "\(heldDeletes ?? 0) deletions are waiting for your confirmation"
        case "offline": return "Host not reachable"
        case "denied": return "Access to this folder was withdrawn"
        case "disk_full": return "This Mac's disk is full"
        default: return detail.isEmpty ? "Something went wrong" : detail
        }
    }

    var needsAttention: Bool { ["conflict", "held", "offline", "denied", "disk_full", "error"].contains(state) }

    var symbol: String {
        switch state {
        case "idle": return "checkmark.circle.fill"
        case "syncing", "starting": return "arrow.triangle.2.circlepath"
        case "paused": return "pause.circle"
        case "conflict", "held": return "exclamationmark.triangle.fill"
        default: return "xmark.octagon.fill"
        }
    }

    var symbolColor: Color {
        switch state {
        case "idle": return .green
        case "syncing", "starting", "paused": return .secondary
        case "conflict", "held": return .orange
        default: return .red
        }
    }
}

struct SyncConflictRow: Identifiable, Equatable {
    var syncId: String
    var name: String
    var path: String
    var hostDeleted: Bool
    var id: String { syncId + "\u{1}" + path }
}

struct AvailableSyncRow: Identifiable, Equatable {
    var id: String
    var name: String
    var access: String
    var files: Int64
    var bytes: Int64
    var backup: String
    var joined: Bool
}

struct HostRow: Identifiable, Equatable {
    var id: String
    var name: String
    var address: String
}

struct BrowseRow: Identifiable, Equatable {
    var name: String
    var path: String
    var isDir: Bool
    var size: Int64
    var state: String
    var pinned: Bool
    var cachedBytes: Int64
    var id: String { path }

    var stateText: String {
        switch state {
        case "cloud": return "In the cloud"
        case "partial": return "Partly on this Mac"
        case "cached": return "On this Mac"
        case "dirty": return "Changed, not uploaded yet"
        case "local": return "On this Mac"
        default: return ""
        }
    }
}

struct VersionRow: Identifiable, Equatable {
    var rev: Int64
    var size: Int64
    var mtime: Int64
    var author: String
    var created: Int64
    var id: Int64 { rev }
}

struct HostedRow: Identifiable, Equatable {
    var id: String
    var name: String
    var root: String
    var backup: String
    var files: Int64
    var bytes: Int64
    var issues: [String]
}

struct ClientRow: Identifiable, Equatable {
    var id: String
    var name: String
    var embed: Bool
    var syncs: [String: String]
}

// MARK: - Model

/// Everything the Sync page shows and does. The state lives in the Rust core; this keeps a copy that is
/// refreshed on events and after every action.
@MainActor
final class SyncModel: ObservableObject {
    @Published var enabled = false
    @Published var loaded = false
    @Published var folders: [SyncFolder] = []
    @Published var conflicts: [SyncConflictRow] = []
    @Published var hosts: [HostRow] = []
    @Published var hosted: [HostedRow] = []
    @Published var clients: [ClientRow] = []
    @Published var hostingRunning = false
    @Published var hostingAddress = ""
    @Published var hostingError = ""
    @Published var deviceName = ""
    @Published var uploadLimit = 0
    @Published var downloadLimit = 0
    @Published var cacheLimitGb: [String: Int] = [:]
    @Published var message = ""
    @Published var busy = false

    /// Called with the folders and the number of conflicts after every refresh (status item, badge).
    var onSummary: (([SyncFolder], Int) -> Void)?

    private var core: CoreClient? { AppState.core }
    private var refreshTask: Task<Void, Never>?

    // MARK: refresh

    func refresh() async {
        guard let core else { return }
        guard let st = try? await core.callAsync("sync_status") else { return }
        enabled = st["enabled"] as? Bool ?? false
        folders = ((st["syncs"] as? [[String: Any]]) ?? []).map(SyncFolder.init(json:))
        if let h = st["hosting"] as? [String: Any] {
            hostingRunning = h.flag("running")
            hostingAddress = h.str("public_addr")
            hostingError = h.str("error")
            hosted = ((h["syncs"] as? [[String: Any]]) ?? []).map {
                HostedRow(id: $0.str("id"), name: $0.str("name"), root: $0.str("root"), backup: $0.str("backup"),
                          files: $0.int("files"), bytes: $0.int("bytes"), issues: ($0["issues"] as? [String]) ?? [])
            }
        } else {
            hosted = []
            hostingRunning = false
        }
        if enabled {
            if let c = try? await core.callAsync("sync_conflicts") {
                conflicts = ((c["conflicts"] as? [[String: Any]]) ?? []).map {
                    SyncConflictRow(syncId: $0.str("sync_id"), name: $0.str("name"), path: $0.str("path"), hostDeleted: $0.flag("host_deleted"))
                }
            }
            if let h = try? await core.callAsync("sync_hosts") {
                hosts = ((h["hosts"] as? [[String: Any]]) ?? []).map { HostRow(id: $0.str("id"), name: $0.str("name"), address: $0.str("address")) }
            }
            if let c = try? await core.callAsync("sync_clients") {
                clients = ((c["clients"] as? [[String: Any]]) ?? []).map {
                    ClientRow(id: $0.str("id"), name: $0.str("name"), embed: $0.flag("embed"), syncs: ($0["syncs"] as? [String: String]) ?? [:])
                }
            }
        } else {
            conflicts = []
            hosts = []
            clients = []
        }
        if let s = try? await core.callAsync("get_settings"), let sync = s["sync"] as? [String: Any] {
            deviceName = sync.str("device_name")
            uploadLimit = Int(sync.int("upload_limit_kbps"))
            downloadLimit = Int(sync.int("download_limit_kbps"))
            var limits: [String: Int] = [:]
            for f in (sync["folders"] as? [[String: Any]]) ?? [] { limits[f.str("sync_id")] = Int(f.int("cache_limit_gb")) }
            cacheLimitGb = limits
        }
        loaded = true
        onSummary?(folders, conflicts.count)
    }

    /// Events from the core: refresh soon, but not once per file.
    func handle(event ev: [String: Any]) {
        guard let t = ev["type"] as? String, t.hasPrefix("sync_") else { return }
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 300_000_000)
            if Task.isCancelled { return }
            await self?.refresh()
        }
    }

    private func run(_ what: String, _ body: () async throws -> Void) async -> Bool {
        busy = true
        defer { busy = false }
        do {
            try await body()
            message = ""
            await refresh()
            return true
        } catch {
            message = error.localizedDescription
            logLine("\(what): \(error.localizedDescription)")
            await refresh()
            return false
        }
    }

    // MARK: on / off, settings

    func setEnabled(_ on: Bool) async {
        _ = await run("sync_enable") { _ = try await self.core?.callAsync("sync_enable", ["enabled": on]) }
    }

    func saveSyncSettings() async {
        guard let core else { return }
        _ = await run("save sync settings") {
            var s = try await core.callAsync("get_settings")
            var sync = s["sync"] as? [String: Any] ?? [:]
            sync["device_name"] = self.deviceName
            sync["upload_limit_kbps"] = max(self.uploadLimit, 0)
            sync["download_limit_kbps"] = max(self.downloadLimit, 0)
            s["sync"] = sync
            _ = try await core.callAsync("set_settings", s)
        }
    }

    // MARK: joining

    /// Redeems an invitation code. Returns the host id.
    func pair(code: String) async -> String? {
        var hostId: String?
        _ = await run("pair") {
            let r = try await self.core!.callAsync("sync_pair", ["code": code.trimmingCharacters(in: .whitespacesAndNewlines)])
            hostId = r["host_id"] as? String
        }
        return hostId
    }

    func available(host: String) async -> [AvailableSyncRow] {
        var out: [AvailableSyncRow] = []
        _ = await run("available") {
            let r = try await self.core!.callAsync("sync_available", ["host": host])
            out = ((r["syncs"] as? [[String: Any]]) ?? []).map {
                AvailableSyncRow(id: $0.str("id"), name: $0.str("name"), access: $0.str("access"), files: $0.int("files"),
                                 bytes: $0.int("bytes"), backup: $0.str("backup"), joined: $0.flag("joined"))
            }
        }
        return out
    }

    func join(host: String, syncId: String, path: String, conflict: String, fullCopy: Bool) async -> Bool {
        await run("join") {
            _ = try await self.core!.callAsync("sync_join", [
                "host": host, "sync_id": syncId, "path": path, "conflict": conflict, "frontend": fullCopy ? "mirror" : "auto",
            ])
        }
    }

    func leave(_ f: SyncFolder, keepFiles: Bool) async {
        _ = await run("leave") { _ = try await self.core!.callAsync("sync_leave", ["sync_id": f.id, "keep_files": keepFiles]) }
    }

    // MARK: per folder

    func setPaused(_ f: SyncFolder, _ paused: Bool) async {
        _ = await run("pause") { _ = try await self.core!.callAsync(paused ? "sync_pause" : "sync_resume", ["sync_id": f.id]) }
    }

    func setAllPaused(_ paused: Bool) async {
        _ = await run("pause all") { _ = try await self.core!.callAsync(paused ? "sync_pause" : "sync_resume") }
    }

    func set(_ f: SyncFolder, _ patch: [String: Any]) async {
        var args = patch
        args["sync_id"] = f.id
        _ = await run("set") { _ = try await self.core!.callAsync("sync_set", args) }
    }

    func verify(_ f: SyncFolder) async {
        _ = await run("verify") { _ = try await self.core!.callAsync("sync_verify", ["sync_id": f.id]) }
    }

    func confirmDeletes(_ f: SyncFolder) async {
        _ = await run("confirm") { _ = try await self.core!.callAsync("sync_confirm_deletes", ["sync_id": f.id]) }
    }

    func resolve(_ c: SyncConflictRow, _ choice: String) async {
        _ = await run("resolve") {
            _ = try await self.core!.callAsync("sync_resolve", ["sync_id": c.syncId, "path": c.path, "choice": choice])
        }
    }

    // MARK: files

    func browse(_ f: SyncFolder, _ path: String) async -> [BrowseRow] {
        guard let core, let r = try? await core.callAsync("sync_browse", ["sync_id": f.id, "path": path]) else { return [] }
        return ((r["items"] as? [[String: Any]]) ?? []).map {
            BrowseRow(name: $0.str("name"), path: $0.str("path"), isDir: $0.flag("is_dir"), size: $0.int("size"),
                      state: $0.str("state"), pinned: $0.flag("pinned"), cachedBytes: $0.int("cached_bytes"))
        }
    }

    func pin(_ f: SyncFolder, _ path: String) async -> String? {
        do {
            _ = try await core!.callAsync("sync_pin", ["sync_id": f.id, "path": path])
            return nil
        } catch { return error.localizedDescription }
    }

    func unpin(_ f: SyncFolder, _ path: String) async -> String? {
        do {
            _ = try await core!.callAsync("sync_unpin", ["sync_id": f.id, "path": path, "evict": true])
            return nil
        } catch { return error.localizedDescription }
    }

    func versions(_ f: SyncFolder, _ path: String) async -> [VersionRow] {
        guard let core, let r = try? await core.callAsync("sync_versions", ["sync_id": f.id, "path": path]) else { return [] }
        return ((r["versions"] as? [[String: Any]]) ?? []).map {
            VersionRow(rev: $0.int("rev"), size: $0.int("size"), mtime: $0.int("mtime"), author: $0.str("author"), created: $0.int("created"))
        }
    }

    func restore(_ f: SyncFolder, path: String, rev: Int64, asCopy: Bool) async -> Bool {
        await run("restore") {
            _ = try await self.core!.callAsync("sync_restore", [
                "sync_id": f.id, "what": ["file": ["path": path, "rev": rev]], "as_copy": asCopy,
            ])
        }
    }

    // MARK: hosting

    func share(name: String, root: String, backup: String) async -> Bool {
        await run("share") { _ = try await self.core!.callAsync("sync_host_add", ["name": name, "root": root, "backup": backup]) }
    }

    func stopSharing(_ h: HostedRow) async {
        _ = await run("stop sharing") { _ = try await self.core!.callAsync("sync_host_remove", ["sync": h.id]) }
    }

    func invite(grants: [(String, String)]) async -> String? {
        var code: String?
        _ = await run("invite") {
            let r = try await self.core!.callAsync("sync_invite", ["grants": grants.map { ["sync": $0.0, "access": $0.1] }])
            code = r["code"] as? String
        }
        return code
    }

    func setAccess(_ c: ClientRow, sync: String, access: String) async {
        _ = await run("grant") { _ = try await self.core!.callAsync("sync_grant", ["client": c.id, "sync": sync, "access": access]) }
    }

    func removeDevice(_ c: ClientRow) async {
        _ = await run("revoke") { _ = try await self.core!.callAsync("sync_revoke", ["client": c.id]) }
    }

    func snapshot(_ h: HostedRow) async {
        _ = await run("snapshot") { _ = try await self.core!.callAsync("sync_host_snapshot", ["sync": h.id]) }
    }

    // MARK: Finder

    static func defaultLocation(for name: String) -> String {
        let safe = name.replacingOccurrences(of: "/", with: "-")
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("MediaLake").appendingPathComponent(safe).path
    }
}
