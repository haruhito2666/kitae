// HistoryManager.swift — persistent browsing history for Kitae.
//
// History items are stored in ~/Library/Application Support/Kitae/history.json.
// Tracks page titles, URLs, and visit timestamps, and supports grouping,
// searching, and deletion.

import Foundation

struct HistoryItem: Codable, Identifiable, Equatable {
    let id: UUID
    var title: String
    let url: String
    let visitedAt: Date

    init(id: UUID = UUID(), title: String, url: String, visitedAt: Date = Date()) {
        self.id = id
        self.title = title
        self.url = url
        self.visitedAt = visitedAt
    }

    var displayTitle: String {
        if !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return title
        }
        if let host = URL(string: url)?.host, !host.isEmpty {
            return host
        }
        return url
    }

    var hostOrURL: String {
        if let host = URL(string: url)?.host, !host.isEmpty {
            return host
        }
        return url
    }
}

final class HistoryManager {
    static let shared = HistoryManager()

    static let historyDidChangeNotification = Notification.Name("KitaeHistoryDidChangeNotification")

    private let lock = NSLock()
    private var items: [HistoryItem] = []
    private let fileURL: URL
    private var saveDebounceTimer: Timer?
    private let persistenceQueue = DispatchQueue(label: "Kitae.History.Persistence", qos: .utility)

    private init() {
        let fileManager = FileManager.default
        let appSupport = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let kitaeDir = appSupport.appendingPathComponent("Kitae", isDirectory: true)
        if !fileManager.fileExists(atPath: kitaeDir.path) {
            try? fileManager.createDirectory(at: kitaeDir, withIntermediateDirectories: true, attributes: nil)
        }
        fileURL = kitaeDir.appendingPathComponent("history.json")
        loadFromDisk()
    }

    // MARK: - Persistence

    private func loadFromDisk() {
        guard FileManager.default.fileExists(atPath: fileURL.path),
              let data = try? Data(contentsOf: fileURL),
              let decoded = try? JSONDecoder().decode([HistoryItem].self, from: data) else {
            return
        }
        items = decoded.sorted(by: { $0.visitedAt > $1.visitedAt })
    }

    private func scheduleSave() {
        DispatchQueue.main.async { [weak self] in
            self?.saveDebounceTimer?.invalidate()
            self?.saveDebounceTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: false) { [weak self] _ in
                self?.saveToDisk()
            }
        }
    }

    private func saveToDisk() {
        lock.lock()
        let snapshot = items
        lock.unlock()

        persistenceQueue.async { [fileURL] in
            if let data = try? JSONEncoder().encode(snapshot) {
                try? data.write(to: fileURL, options: .atomic)
            }
        }
    }

    // MARK: - Query

    func recent(limit: Int = 10) -> [HistoryItem] {
        lock.lock()
        defer { lock.unlock() }
        return Array(items.prefix(limit))
    }

    func allItems() -> [HistoryItem] {
        lock.lock()
        defer { lock.unlock() }
        return items
    }

    func search(query: String) -> [HistoryItem] {
        let q = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        lock.lock()
        defer { lock.unlock() }
        if q.isEmpty { return items }
        return items.filter {
            $0.title.lowercased().contains(q) || $0.url.lowercased().contains(q)
        }
    }

    /// Group items by relative date sections: "Today", "Yesterday", "Earlier this week", "Earlier this month", "Older"
    func grouped(query: String = "") -> [(section: String, items: [HistoryItem])] {
        let filtered = search(query: query)
        let calendar = Calendar.current
        let now = Date()

        var today: [HistoryItem] = []
        var yesterday: [HistoryItem] = []
        var thisWeek: [HistoryItem] = []
        var thisMonth: [HistoryItem] = []
        var older: [HistoryItem] = []

        for item in filtered {
            if calendar.isDateInToday(item.visitedAt) {
                today.append(item)
            } else if calendar.isDateInYesterday(item.visitedAt) {
                yesterday.append(item)
            } else if let diff = calendar.dateComponents([.day], from: item.visitedAt, to: now).day, diff < 7 {
                thisWeek.append(item)
            } else if let diff = calendar.dateComponents([.day], from: item.visitedAt, to: now).day, diff < 30 {
                thisMonth.append(item)
            } else {
                older.append(item)
            }
        }

        var result: [(String, [HistoryItem])] = []
        if !today.isEmpty { result.append(("Today", today)) }
        if !yesterday.isEmpty { result.append(("Yesterday", yesterday)) }
        if !thisWeek.isEmpty { result.append(("Earlier this week", thisWeek)) }
        if !thisMonth.isEmpty { result.append(("Earlier this month", thisMonth)) }
        if !older.isEmpty { result.append(("Older", older)) }
        return result
    }

    // MARK: - Mutations

    func add(title: String, url: String) {
        let cleanURL = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !cleanURL.isEmpty,
              cleanURL != "about:blank",
              cleanURL != "about:start",
              cleanURL != "about:history",
              cleanURL != "kitae://history" else {
            return
        }

        lock.lock()
        // If the top item is the same URL within 10 seconds, update title and time instead of duplicate
        if let first = items.first, first.url == cleanURL, abs(first.visitedAt.timeIntervalSinceNow) < 10 {
            if !title.isEmpty && title != cleanURL {
                items[0].title = title
            }
            lock.unlock()
            notifyChange()
            scheduleSave()
            return
        }

        let cleanTitle = title.isEmpty ? cleanURL : title
        let newItem = HistoryItem(title: cleanTitle, url: cleanURL, visitedAt: Date())
        items.insert(newItem, at: 0)

        // Keep maximum 2000 entries
        if items.count > 2000 {
            items.removeLast(items.count - 2000)
        }
        lock.unlock()

        notifyChange()
        scheduleSave()
    }

    func delete(id: UUID) {
        lock.lock()
        items.removeAll(where: { $0.id == id })
        lock.unlock()

        notifyChange()
        scheduleSave()
    }

    func clearAll() {
        clearAllNow()
    }

    func clearAllNow() {
        lock.lock()
        items.removeAll()
        lock.unlock()

        saveDebounceTimer?.invalidate()
        saveDebounceTimer = nil
        persistenceQueue.sync { [fileURL] in
            if let data = try? JSONEncoder().encode([HistoryItem]()) {
                try? data.write(to: fileURL, options: .atomic)
            }
        }
        notifyChange()
    }

    private func notifyChange() {
        DispatchQueue.main.async {
            NotificationCenter.default.post(name: Self.historyDidChangeNotification, object: nil)
        }
    }
}
