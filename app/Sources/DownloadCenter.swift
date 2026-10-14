import AppKit

// H1: process-wide download ledger. Each tab's engine narrates only the
// downloads it started (the engine-side registry is owned per-Browser), so
// every stream funnels here and rows merge by engine id — two tabs can never
// collide because ids come from the same process-global counter.

final class DownloadCenter {
    static let shared = DownloadCenter()

    /// Posted on the main thread after every mutation; UI observes this.
    static let didChangeNotification = Notification.Name("kitae.downloadCenterDidChange")

    /// Oldest first, matching the engine's registry order.
    private(set) var items: [DownloadItem] = []

    private init() {}

    var activeCount: Int {
        items.filter(\.isActive).count
    }

    var hasAny: Bool { !items.isEmpty }

    /// Merge one engine notification into the ledger. Updating an unknown id
    /// appends it (first sight = row creation); known ids update in place and
    /// keep the earlier total when the engine reports an empty one.
    @discardableResult
    func apply(_ update: DownloadItem) -> DownloadItem {
        var merged: DownloadItem?
        if let idx = items.firstIndex(where: { $0.id == update.id }) {
            var row = items[idx]
            row.state = update.state
            row.received = update.received
            if update.total != nil { row.total = update.total }
            items[idx] = row
            merged = row
        } else {
            items.append(update)
            merged = update
        }
        DispatchQueue.main.async {
            NotificationCenter.default.post(name: DownloadCenter.didChangeNotification, object: self)
        }
        return merged!
    }

    /// Size text for a row — "1.2 MB of 4.5 MB", or just the received amount
    /// when the server did not send a length.
    static func sizeText(for item: DownloadItem) -> String {
        let fmt: (UInt64) -> String = { bytes in
            ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
        }
        if let total = item.total {
            return "\(fmt(item.received)) of \(fmt(total))"
        }
        return fmt(item.received)
    }
}
