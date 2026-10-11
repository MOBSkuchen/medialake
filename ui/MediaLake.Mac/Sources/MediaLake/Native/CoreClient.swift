import CMediaLakeCore
import Foundation

struct CoreError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
    init(_ message: String) { self.message = message }
}

/// Safe wrapper over the C ABI (JSON across the boundary). Mirrors `CoreClient.cs`.
final class CoreClient: @unchecked Sendable {
    nonisolated(unsafe) private static var instance: CoreClient?

    private var handle: OpaquePointer?

    /// Delivered on the main thread (the core calls back on a Rust thread).
    var onEvent: (([String: Any]) -> Void)?

    private init(handle: OpaquePointer) { self.handle = handle }

    static func version() -> String { take(sn_version()) }

    static func create(configDir: String?, options: [String: Any]) throws -> CoreClient {
        let opts = (try? JSONSerialization.data(withJSONObject: options)).flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
        guard let h = sn_init(configDir ?? "", opts) else { throw CoreError(take(sn_last_error())) }
        let c = CoreClient(handle: h)
        instance = c
        sn_set_event_callback(h) { (msg: UnsafePointer<CChar>?) in
            guard let msg, let s = String(validatingUTF8: msg), let data = s.data(using: .utf8),
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
            DispatchQueue.main.async { CoreClient.instance?.onEvent?(obj) }
        }
        return c
    }

    /// Blocking call; use `callAsync` from UI code.
    func call(_ method: String, _ args: [String: Any]? = nil) throws -> Any {
        guard let h = handle else { throw CoreError("core is shut down") }
        let argsJson = args.flatMap { try? JSONSerialization.data(withJSONObject: $0) }.flatMap { String(data: $0, encoding: .utf8) } ?? ""
        let json = CoreClient.take(sn_call(h, method, argsJson))
        guard let data = json.data(using: .utf8), let root = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw CoreError("bad response from the core")
        }
        if let err = root["error"] as? String { throw CoreError(err) }
        return root["ok"] ?? [String: Any]()
    }

    func callObject(_ method: String, _ args: [String: Any]? = nil) throws -> [String: Any] {
        (try call(method, args)) as? [String: Any] ?? [:]
    }

    func callAsync(_ method: String, _ args: [String: Any]? = nil) async throws -> [String: Any] {
        try await withCheckedThrowingContinuation { cont in
            DispatchQueue.global(qos: .userInitiated).async {
                do { cont.resume(returning: try self.callObject(method, args)) } catch { cont.resume(throwing: error) }
            }
        }
    }

    func dispose() {
        guard let h = handle else { return }
        handle = nil
        sn_set_event_callback(h, nil)
        sn_shutdown(h)
        CoreClient.instance = nil
    }

    private static func take(_ p: UnsafeMutablePointer<CChar>?) -> String {
        guard let p else { return "" }
        defer { sn_free_string(p) }
        return String(cString: p)
    }
}
