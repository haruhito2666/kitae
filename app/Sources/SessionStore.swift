// SessionStore.swift — H7: persistent session for Kitae.
//
// The session lives in ~/Library/Application Support/Kitae/session.json:
// the non-start tab URLs and the active index among them. Saved (debounced)
// whenever the tab set changes and synchronously at termination; restored
// on the next launch before the first tab is created. A session that ended
// with only start pages open deletes the file, so the next launch starts
// fresh instead of "restoring" empty tabs.

import Foundation

struct SessionState: Codable, Equatable {
    var urls: [String]
    var activeIndex: Int
}

final class SessionStore {
    static let shared = SessionStore()

    private let fileURL: URL
    private var debounceTimer: Timer?

    private init() {
        let fileManager = FileManager.default
        let appSupport = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let kitaeDir = appSupport.appendingPathComponent("Kitae", isDirectory: true)
        if !fileManager.fileExists(atPath: kitaeDir.path) {
            try? fileManager.createDirectory(at: kitaeDir, withIntermediateDirectories: true, attributes: nil)
        }
        fileURL = kitaeDir.appendingPathComponent("session.json")
    }

    /// Debounced save for tab-set churn (main thread; each call reschedules
    /// with the latest snapshot, so only the settled state hits the disk).
    func save(urls: [String], activeIndex: Int) {
        debounceTimer?.invalidate()
        debounceTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: false) { [weak self] _ in
            self?.saveNow(urls: urls, activeIndex: activeIndex)
        }
    }

    /// Immediate save (termination; also the debounce's landing).
    func saveNow(urls: [String], activeIndex: Int) {
        debounceTimer?.invalidate()
        guard !urls.isEmpty else {
            try? FileManager.default.removeItem(at: fileURL)
            return
        }
        let state = SessionState(urls: urls, activeIndex: max(0, activeIndex))
        if let data = try? JSONEncoder().encode(state) {
            try? data.write(to: fileURL, options: .atomic)
        }
    }

    /// The saved session, if any (nil = first launch or fresh last session).
    func load() -> SessionState? {
        guard let data = try? Data(contentsOf: fileURL),
              let state = try? JSONDecoder().decode(SessionState.self, from: data),
              !state.urls.isEmpty else { return nil }
        return state
    }
}
