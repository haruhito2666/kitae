import Foundation

final class HstsStore {
    static let shared = HstsStore()

    private let fileURL: URL

    private init() {
        let fileManager = FileManager.default
        let appSupport = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let directory = appSupport.appendingPathComponent("Kitae", isDirectory: true)
        try? fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        fileURL = directory.appendingPathComponent("hsts.bin")
    }

    func restore() {
        guard let data = try? Data(contentsOf: fileURL) else { return }
        _ = data.withUnsafeBytes { bytes in
            kitae_hsts_import_snapshot(
                bytes.bindMemory(to: UInt8.self).baseAddress,
                data.count
            )
        }
    }

    func saveNow() {
        let size = kitae_hsts_snapshot_size()
        guard size > 8 else {
            try? FileManager.default.removeItem(at: fileURL)
            return
        }
        var bytes = [UInt8](repeating: 0, count: size)
        let written = bytes.withUnsafeMutableBytes { buffer in
            kitae_hsts_snapshot(buffer.baseAddress, buffer.count)
        }
        guard written == size else { return }
        try? Data(bytes).write(to: fileURL, options: .atomic)
    }

    func clear() {
        kitae_clear_all_hsts()
        try? FileManager.default.removeItem(at: fileURL)
    }
}
