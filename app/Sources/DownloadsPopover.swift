import AppKit

// H1: the Downloads panel — a transient popover listing every download the
// session has seen (all tabs merge through DownloadCenter). Active rows show
// a determinate progress bar when the server sent a length, indeterminate
// otherwise; finished rows offer Show in Finder.

final class DownloadsPopover: NSObject, NSPopoverDelegate {
    static let shared = DownloadsPopover()

    private let popover = NSPopover()
    private let container = DownloadsContainerView()
    private var changeObserver: Any?

    override init() {
        super.init()
        popover.behavior = .transient
        popover.animates = true
        popover.delegate = self

        let vc = NSViewController()
        vc.view = container
        popover.contentViewController = vc

        container.onResize = { [weak self] newSize in
            self?.popover.contentSize = newSize
        }

        changeObserver = NotificationCenter.default.addObserver(
            forName: DownloadCenter.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.container.rebuild()
        }
    }

    deinit {
        if let obs = changeObserver {
            NotificationCenter.default.removeObserver(obs)
        }
    }

    var isShown: Bool { popover.isShown }

    func toggle(relativeTo positioningView: NSView) {
        if popover.isShown {
            dismiss()
        } else {
            present(relativeTo: positioningView)
        }
    }

    func present(relativeTo positioningView: NSView) {
        container.rebuild()
        popover.contentSize = container.fittingSize
        popover.show(relativeTo: positioningView.bounds, of: positioningView, preferredEdge: .maxY)
    }

    func dismiss() {
        if popover.isShown {
            popover.performClose(nil)
        }
    }
}

// MARK: - Container

final class DownloadsContainerView: FlippedView {
    private let headerLabel = NSTextField(labelWithString: "Downloads")
    private let emptyLabel = NSTextField(labelWithString: "No downloads yet")
    private let emptySub = NSTextField(labelWithString: "Files you download will appear here.")

    override init(frame: NSRect) {
        super.init(frame: NSRect(x: 0, y: 0, width: 360, height: 132))
        wantsLayer = true

        headerLabel.font = NSFont.systemFont(ofSize: 15, weight: .bold)
        headerLabel.textColor = Theme.chromeText
        addSubview(headerLabel)

        emptyLabel.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        emptyLabel.textColor = Theme.chromeText
        addSubview(emptyLabel)

        emptySub.font = NSFont.systemFont(ofSize: 11, weight: .regular)
        emptySub.textColor = Theme.chromeMutedText
        addSubview(emptySub)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func rebuild() {
        // Rows are rebuilt wholesale — the ledger changes at most ~10×/s per
        // active download, and row views are cheap.
        subviews.forEach { $0.removeFromSuperview() }
        addSubview(headerLabel)

        let items = DownloadCenter.shared.items
        if items.isEmpty {
            addSubview(emptyLabel)
            addSubview(emptySub)
        } else {
            // Newest last so the oldest sits at the top; cap the panel height.
            let shown = items.suffix(8)
            var y: CGFloat = 40
            for item in shown {
                let row = DownloadRowView(item: item)
                row.frame = NSRect(x: 8, y: y, width: 344, height: 48)
                addSubview(row)
                y += 52
            }
        }

        let target = fittingSize
        frame.size = target
        onResize?(target)
    }

    var onResize: ((NSSize) -> Void)?

    override var fittingSize: NSSize {
        let items = DownloadCenter.shared.items
        let rowCount = min(items.count, 8)
        let height: CGFloat = items.isEmpty ? 96 : 40 + CGFloat(rowCount) * 52 + 8
        return NSSize(width: 360, height: height)
    }

    override func layout() {
        super.layout()
        headerLabel.frame = NSRect(x: 14, y: 12, width: 240, height: 22)
        emptyLabel.frame = NSRect(x: 14, y: 38, width: 332, height: 18)
        emptySub.frame = NSRect(x: 14, y: 58, width: 332, height: 16)
    }
}

// MARK: - Row

final class DownloadRowView: FlippedView {
    private let item: DownloadItem
    private let nameLabel = NSTextField(labelWithString: "")
    private let statusLabel = NSTextField(labelWithString: "")
    private let progress = NSProgressIndicator()
    private let revealButton = NSButton(title: "Show in Finder", target: nil, action: nil)
    private var animating = false

    init(item: DownloadItem) {
        self.item = item
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.backgroundColor = Theme.cardBackground.cgColor

        nameLabel.stringValue = item.filename
        nameLabel.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        nameLabel.textColor = Theme.chromeText
        nameLabel.lineBreakMode = .byTruncatingMiddle
        addSubview(nameLabel)

        statusLabel.font = Theme.fontCellURL
        statusLabel.textColor = Theme.chromeMutedText
        statusLabel.lineBreakMode = .byTruncatingTail
        addSubview(statusLabel)

        progress.isIndeterminate = item.total == nil
        progress.minValue = 0
        progress.maxValue = 1
        progress.doubleValue = item.fraction ?? 0
        progress.controlSize = .small
        addSubview(progress)

        if item.isFinished || item.isFailed {
            if item.isFinished {
                statusLabel.stringValue = "Done — \(DownloadCenter.sizeText(for: item))"
            } else {
                statusLabel.stringValue = item.failureMessage ?? "Download failed"
                statusLabel.textColor = Theme.accent
            }
            revealButton.bezelStyle = .roundRect
            revealButton.controlSize = .small
            revealButton.font = NSFont.systemFont(ofSize: 10, weight: .medium)
            revealButton.target = self
            revealButton.action = #selector(revealInFinder)
            addSubview(revealButton)
        } else {
            statusLabel.stringValue = DownloadCenter.sizeText(for: item)
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func revealInFinder() {
        let path = item.path
        DispatchQueue.global(qos: .userInitiated).async {
            if !FileManager.default.fileExists(atPath: path) { return }
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
        }
    }

    override func layout() {
        super.layout()
        let w = bounds.width
        let showReveal = revealButton.superview != nil
        let textWidth = showReveal ? w - 16 - 108 : w - 16

        nameLabel.frame = NSRect(x: 8, y: 8, width: textWidth, height: 16)
        statusLabel.frame = NSRect(x: 8, y: 26, width: textWidth, height: 14)

        if showReveal {
            revealButton.frame = NSRect(x: w - 104, y: 14, width: 96, height: 20)
        } else if item.total == nil {
            // Indeterminate barber-pole across the row's lower half.
            progress.frame = NSRect(x: 8, y: 38, width: w - 16, height: 4)
            if !animating {
                animating = true
                progress.startAnimation(nil)
            }
        } else {
            progress.frame = NSRect(x: 8, y: 38, width: w - 16, height: 4)
        }
    }
}
