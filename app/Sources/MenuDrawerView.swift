// MenuDrawerView.swift — right-side slide-over drawer for Kitae.
//
// Displays:
// - Top-right Settings button
// - Beautifully organized Recent History list (monograms, titles, timestamps, hover delete)
// - "See Full History" button linking to the dedicated history page

import AppKit

final class MenuDrawerView: NSView {
    var onOpenSettings: (() -> Void)?
    var onOpenFullHistory: (() -> Void)?
    var onNavigate: ((String) -> Void)?
    var onClose: (() -> Void)?

    let settingsButton = GlyphButton(symbolName: "gearshape")

    private let headerLabel = NSTextField(labelWithString: "Menu")
    private let historyHeaderLabel = NSTextField(labelWithString: "RECENT HISTORY")
    private let clearRecentButton = GlyphButton(symbolName: "trash", glyphSize: 10)

    private let scrollView = NSScrollView()
    private let stackView = NSStackView()
    private let emptyStateLabel = NSTextField(labelWithString: "No recent history yet")
    private let emptyStateDetail = NSTextField(labelWithString: "Pages you visit will appear here.")

    private let fullHistoryRow = FullHistoryActionRow()

    private var rows: [RecentHistoryRowView] = []

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        refreshBackground()

        setupViews()
        reloadHistory()

        NotificationCenter.default.addObserver(
            self,
            selector: #selector(historyChanged),
            name: HistoryManager.historyDidChangeNotification,
            object: nil
        )
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    private func refreshBackground() {
        layer?.backgroundColor = Theme.cgColor(of: Theme.dynamic(
            NSColor(white: 1.0, alpha: 0.16),
            NSColor(white: 1.0, alpha: 0.05)
        ), for: effectiveAppearance)
    }

    private func setupViews() {
        // Header
        headerLabel.font = NSFont.systemFont(ofSize: 15, weight: .semibold)
        headerLabel.textColor = Theme.chromeText
        addSubview(headerLabel)

        settingsButton.action = { [weak self] in self?.onOpenSettings?() }
        addSubview(settingsButton)

        // Recent History section header
        historyHeaderLabel.font = NSFont.systemFont(ofSize: 10.5, weight: .bold)
        historyHeaderLabel.textColor = Theme.chromeMutedText
        addSubview(historyHeaderLabel)

        clearRecentButton.action = { [weak self] in
            HistoryManager.shared.clearAll()
            self?.reloadHistory()
        }
        addSubview(clearRecentButton)

        // Scrollable stack for recent history
        scrollView.hasVerticalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        addSubview(scrollView)

        stackView.orientation = .vertical
        stackView.spacing = 3
        stackView.alignment = .leading
        stackView.edgeInsets = NSEdgeInsets(top: 2, left: 10, bottom: 2, right: 10)
        scrollView.documentView = stackView

        // Empty state labels
        emptyStateLabel.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        emptyStateLabel.textColor = Theme.chromeMutedText
        emptyStateLabel.alignment = .center
        addSubview(emptyStateLabel)

        emptyStateDetail.font = Theme.fontCellURL
        emptyStateDetail.textColor = Theme.chromeMutedText.withAlphaComponent(0.7)
        emptyStateDetail.alignment = .center
        addSubview(emptyStateDetail)

        // Bottom action: See Full History
        fullHistoryRow.action = { [weak self] in self?.onOpenFullHistory?() }
        addSubview(fullHistoryRow)
    }

    @objc private func historyChanged() {
        reloadHistory()
    }

    func reloadHistory() {
        let items = HistoryManager.shared.recent(limit: 8)

        for row in rows { row.removeFromSuperview() }
        rows.removeAll()

        let isEmpty = items.isEmpty
        emptyStateLabel.isHidden = !isEmpty
        emptyStateDetail.isHidden = !isEmpty
        scrollView.isHidden = isEmpty
        clearRecentButton.isHidden = isEmpty

        if !isEmpty {
            for item in items {
                let row = RecentHistoryRowView(item: item)
                row.onSelect = { [weak self] in
                    self?.onNavigate?(item.url)
                    self?.onClose?()
                }
                row.onDelete = { [weak self] in
                    HistoryManager.shared.delete(id: item.id)
                    self?.reloadHistory()
                }
                stackView.addArrangedSubview(row)
                rows.append(row)
            }
        }
        needsLayout = true
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refreshBackground()
    }

    override func layout() {
        super.layout()
        let w = bounds.width
        let h = bounds.height

        let headerHeight: CGFloat = 48
        headerLabel.frame = NSRect(x: 16, y: (headerHeight - 20) / 2, width: 140, height: 20)
        let buttonSide: CGFloat = 28
        settingsButton.frame = NSRect(x: w - buttonSide - 14, y: (headerHeight - buttonSide) / 2, width: buttonSide, height: buttonSide)

        let sectionHeaderY = headerHeight + 8
        historyHeaderLabel.frame = NSRect(x: 16, y: sectionHeaderY, width: 160, height: 16)
        clearRecentButton.frame = NSRect(x: w - 24 - 14, y: sectionHeaderY - 2, width: 22, height: 20)

        let bottomRowHeight: CGFloat = 56
        let bottomY = h - bottomRowHeight
        fullHistoryRow.frame = NSRect(x: 12, y: bottomY + (bottomRowHeight - 38) / 2, width: w - 24, height: 38)

        let listTop = sectionHeaderY + 24
        let listHeight = max(0, bottomY - listTop - 6)
        scrollView.frame = NSRect(x: 0, y: listTop, width: w, height: listHeight)
        stackView.frame = NSRect(x: 0, y: 0, width: w, height: max(listHeight, stackView.fittingSize.height))

        emptyStateLabel.frame = NSRect(x: 20, y: listTop + 60, width: w - 40, height: 18)
        emptyStateDetail.frame = NSRect(x: 20, y: listTop + 82, width: w - 40, height: 16)
    }
}

// MARK: - Recent History Item Row

final class RecentHistoryRowView: NSView {
    let item: HistoryItem
    var onSelect: (() -> Void)?
    var onDelete: (() -> Void)?

    private let monogram = MonogramView(radius: 7)
    private let titleLabel = NSTextField(labelWithString: "")
    private let timeLabel = NSTextField(labelWithString: "")
    private let urlLabel = NSTextField(labelWithString: "")
    private let deleteButton = GlyphButton(symbolName: "xmark", glyphSize: 8) {}

    private var isHovered = false

    init(item: HistoryItem) {
        self.item = item
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = Theme.cornerRadius

        monogram.configure(title: item.displayTitle)
        addSubview(monogram)

        titleLabel.stringValue = item.displayTitle
        titleLabel.font = Theme.fontTabLabel
        titleLabel.textColor = Theme.chromeText
        titleLabel.lineBreakMode = .byTruncatingTail
        addSubview(titleLabel)

        timeLabel.stringValue = Self.formatTime(item.visitedAt)
        timeLabel.font = NSFont.systemFont(ofSize: 10, weight: .regular)
        timeLabel.textColor = Theme.chromeMutedText
        timeLabel.alignment = .right
        addSubview(timeLabel)

        urlLabel.stringValue = item.hostOrURL
        urlLabel.font = Theme.fontCellURL
        urlLabel.textColor = Theme.chromeMutedText
        urlLabel.lineBreakMode = .byTruncatingTail
        addSubview(urlLabel)

        deleteButton.action = { [weak self] in self?.onDelete?() }
        deleteButton.isHidden = true
        addSubview(deleteButton)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    private static func formatTime(_ date: Date) -> String {
        let now = Date()
        let seconds = Int(now.timeIntervalSince(date))
        if seconds < 60 { return "Just now" }
        if seconds < 3600 { return "\(seconds / 60)m ago" }
        if seconds < 86400 {
            let formatter = DateFormatter()
            formatter.dateFormat = "h:mm a"
            return formatter.string(from: date)
        }
        let formatter = DateFormatter()
        formatter.dateFormat = "MMM d"
        return formatter.string(from: date)
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        let w = bounds.width

        let monoSide: CGFloat = 20
        monogram.frame = NSRect(x: 8, y: (h - monoSide) / 2, width: monoSide, height: monoSide)

        let textX = monogram.frame.maxX + 8
        let timeWidth: CGFloat = 58
        let closeWidth: CGFloat = 18

        deleteButton.frame = NSRect(x: w - closeWidth - 6, y: (h - closeWidth) / 2, width: closeWidth, height: closeWidth)

        let trailingRight = isHovered ? (deleteButton.frame.minX - 4) : (w - 8)
        let availableTitleWidth = max(40, trailingRight - textX - (isHovered ? 0 : timeWidth))

        titleLabel.frame = NSRect(x: textX, y: 7, width: availableTitleWidth, height: 16)
        if !isHovered {
            timeLabel.frame = NSRect(x: w - timeWidth - 8, y: 7, width: timeWidth, height: 16)
            timeLabel.isHidden = false
        } else {
            timeLabel.isHidden = true
        }

        urlLabel.frame = NSRect(x: textX, y: 24, width: w - textX - 16, height: 14)
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: NSView.noIntrinsicMetric, height: 44)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        deleteButton.isHidden = false
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(of: Theme.dynamic(
            NSColor(white: 1.0, alpha: 0.45),
            NSColor(white: 1.0, alpha: 0.10)
        ), for: effectiveAppearance)
        layer?.shadowColor = NSColor.black.cgColor
        layer?.shadowOpacity = 0.05
        layer?.shadowOffset = CGSize(width: 0, height: -1)
        layer?.shadowRadius = 4
        layer?.borderWidth = 1
        layer?.borderColor = Theme.cgColor(of: Theme.dynamic(
            NSColor(white: 1.0, alpha: 0.55),
            NSColor(white: 1.0, alpha: 0.12)
        ), for: effectiveAppearance)
        CATransaction.commit()
        needsLayout = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        deleteButton.isHidden = true
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = NSColor.clear.cgColor
        layer?.shadowOpacity = 0
        layer?.borderWidth = 0
        CATransaction.commit()
        needsLayout = true
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        if !deleteButton.frame.contains(point) {
            onSelect?()
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - "See Full History" Action Row

final class FullHistoryActionRow: NSView {
    var action: () -> Void = {}

    private let icon = NSImageView()
    private let label = NSTextField(labelWithString: "See Full History")
    private let arrow = NSImageView()
    private var isHovered = false
    private var isPressed = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = Theme.cornerRadius
        layer?.masksToBounds = false
        layer?.shadowColor = NSColor.black.cgColor
        layer?.shadowOpacity = 0.05
        layer?.shadowOffset = CGSize(width: 0, height: -2)
        layer?.shadowRadius = 6

        if let clockImg = NSImage(systemSymbolName: "clock.arrow.circlepath", accessibilityDescription: nil) {
            icon.image = clockImg
            icon.contentTintColor = Theme.accent
        }
        addSubview(icon)

        label.font = Theme.fontButtonMedium
        label.textColor = Theme.chromeText
        addSubview(label)

        if let chevronImg = NSImage(systemSymbolName: "chevron.right", accessibilityDescription: nil) {
            arrow.image = chevronImg
            arrow.contentTintColor = Theme.chromeMutedText
        }
        addSubview(arrow)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let h = bounds.height
        let w = bounds.width

        icon.frame = NSRect(x: 12, y: (h - 16) / 2, width: 16, height: 16)
        label.frame = label.centeredTextFrame(x: 36, width: w - 70, inHeight: h)
        arrow.frame = NSRect(x: w - 24, y: (h - 12) / 2, width: 12, height: 12)
    }

    override func draw(_ dirtyRect: NSRect) {
        let fill: NSColor
        if isPressed {
            fill = Theme.pressFill
        } else if isHovered {
            fill = Theme.hoverFill
        } else {
            fill = Theme.dynamic(
                NSColor(white: 1.0, alpha: 0.35),
                NSColor(white: 1.0, alpha: 0.08)
            )
        }
        fill.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: Theme.cornerRadius, yRadius: Theme.cornerRadius).fill()

        let strokeColor = Theme.dynamic(
            NSColor(white: 1.0, alpha: 0.65),
            Theme.addressFieldBorder
        )
        strokeColor.setStroke()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: Theme.cornerRadius, yRadius: Theme.cornerRadius).stroke()
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        layer?.shadowOpacity = 0.09
        layer?.shadowRadius = 8
        Theme.animate { needsDisplay = true }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        layer?.shadowOpacity = 0.05
        layer?.shadowRadius = 6
        Theme.animate { needsDisplay = true }
    }

    override func mouseDown(with event: NSEvent) {
        isPressed = true
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        let wasInside = isPressed
        isPressed = false
        needsDisplay = true
        if wasInside, bounds.contains(convert(event.locationInWindow, from: nil)) {
            Theme.bounce(self)
            action()
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
