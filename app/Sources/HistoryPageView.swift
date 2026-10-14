// HistoryPageView.swift — dedicated full browsing history page for Kitae.
//
// Displayed when the user navigates to about:history or clicks "See Full History"
// in the slide menu drawer. Provides:
// - Real-time search filter
// - Grouping by date (Today, Yesterday, Earlier this week, etc.)
// - Direct link opening & "Open in New Tab"
// - "Copy Link"
// - Item deletion & "Clear Browsing Data"

import AppKit

final class HistoryPageView: NSView, NSSearchFieldDelegate {
    var onNavigate: ((String) -> Void)?
    var onOpenNewTab: ((String) -> Void)?

    private let scrollView = NSScrollView()
    private let contentContainer = NSView()

    private let titleLabel = NSTextField(labelWithString: "History")
    private let countLabel = NSTextField(labelWithString: "")
    private let searchField = NSSearchField()
    private let clearAllButton = PillButton(title: "Clear Browsing Data…", symbolName: "trash")

    private let stackView = NSStackView()
    private let emptyStateView = NSView()
    private let emptyTitle = NSTextField(labelWithString: "No Browsing History")
    private let emptyDetail = NSTextField(labelWithString: "Websites you visit will be saved here.")

    private var currentQuery = ""

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = Theme.cgColor(of: Theme.cardBackground, for: effectiveAppearance)

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

    private func setupViews() {
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        addSubview(scrollView)

        contentContainer.wantsLayer = true
        scrollView.documentView = contentContainer

        // Title & Stats
        titleLabel.font = NSFont.systemFont(ofSize: 28, weight: .bold)
        titleLabel.textColor = Theme.chromeText
        contentContainer.addSubview(titleLabel)

        countLabel.font = NSFont.systemFont(ofSize: 13, weight: .regular)
        countLabel.textColor = Theme.chromeMutedText
        contentContainer.addSubview(countLabel)

        // Clear All
        clearAllButton.action = { [weak self] in self?.confirmClearHistory() }
        contentContainer.addSubview(clearAllButton)

        // Search Field
        searchField.placeholderString = "Search history…"
        searchField.font = Theme.fontAddress
        searchField.focusRingType = .none
        searchField.delegate = self
        searchField.target = self
        searchField.action = #selector(searchSubmitted)
        contentContainer.addSubview(searchField)

        // Stack view for sections
        stackView.orientation = .vertical
        stackView.spacing = 20
        stackView.alignment = .leading
        contentContainer.addSubview(stackView)

        // Empty state
        emptyTitle.font = NSFont.systemFont(ofSize: 16, weight: .semibold)
        emptyTitle.textColor = Theme.chromeText
        emptyTitle.alignment = .center
        emptyDetail.font = Theme.fontSettingsRowDetail
        emptyDetail.textColor = Theme.chromeMutedText
        emptyDetail.alignment = .center
        emptyStateView.addSubview(emptyTitle)
        emptyStateView.addSubview(emptyDetail)
        contentContainer.addSubview(emptyStateView)
    }

    @objc private func historyChanged() {
        reloadHistory()
    }

    func controlTextDidChange(_ obj: Notification) {
        currentQuery = searchField.stringValue
        reloadHistory()
    }

    @objc private func searchSubmitted() {
        currentQuery = searchField.stringValue
        reloadHistory()
    }

    private func reloadHistory() {
        let groups = HistoryManager.shared.grouped(query: currentQuery)
        let totalCount = HistoryManager.shared.allItems().count

        countLabel.stringValue = "\(totalCount) \(totalCount == 1 ? "page" : "pages") visited"

        // Clear previous arranged subviews
        for subview in stackView.arrangedSubviews {
            stackView.removeArrangedSubview(subview)
            subview.removeFromSuperview()
        }

        let isEmpty = groups.isEmpty
        emptyStateView.isHidden = !isEmpty
        stackView.isHidden = isEmpty

        if isEmpty {
            if !currentQuery.isEmpty {
                emptyTitle.stringValue = "No Results Found"
                emptyDetail.stringValue = "No history matches \"\(currentQuery)\""
            } else {
                emptyTitle.stringValue = "No Browsing History"
                emptyDetail.stringValue = "Websites you visit will be saved here."
            }
        } else {
            for group in groups {
                let sectionView = HistorySectionView(sectionTitle: group.section, items: group.items)
                sectionView.onNavigate = { [weak self] url in self?.onNavigate?(url) }
                sectionView.onOpenNewTab = { [weak self] url in self?.onOpenNewTab?(url) }
                sectionView.onDelete = { [weak self] in self?.reloadHistory() }
                stackView.addArrangedSubview(sectionView)
            }
        }

        needsLayout = true
    }

    private func confirmClearHistory() {
        guard let window else { return }
        let alert = NSAlert()
        alert.messageText = "Clear Browsing Data"
        alert.informativeText = "Are you sure you want to clear your entire browsing history? This action cannot be undone."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Clear History")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { response in
            if response == .alertFirstButtonReturn {
                HistoryManager.shared.clearAll()
            }
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        layer?.backgroundColor = Theme.cgColor(of: Theme.cardBackground, for: effectiveAppearance)
    }

    override func layout() {
        super.layout()
        scrollView.frame = bounds

        let maxContentWidth: CGFloat = 820
        let sidePadding = max(24, (bounds.width - maxContentWidth) / 2)
        let contentWidth = bounds.width - sidePadding * 2

        var y: CGFloat = 36
        titleLabel.frame = NSRect(x: sidePadding, y: y, width: 220, height: 34)
        clearAllButton.frame = NSRect(x: bounds.width - sidePadding - 170, y: y + 2, width: 170, height: Theme.pillHeight)
        y += 38

        countLabel.frame = NSRect(x: sidePadding, y: y, width: 300, height: 18)
        y += 28

        searchField.frame = NSRect(x: sidePadding, y: y, width: contentWidth, height: 32)
        y += 46

        let stackHeight = stackView.fittingSize.height
        stackView.frame = NSRect(x: sidePadding, y: y, width: contentWidth, height: stackHeight)

        emptyStateView.frame = NSRect(x: sidePadding, y: y + 40, width: contentWidth, height: 100)
        emptyTitle.frame = NSRect(x: 0, y: 16, width: contentWidth, height: 22)
        emptyDetail.frame = NSRect(x: 0, y: 44, width: contentWidth, height: 18)

        let totalHeight = max(bounds.height, y + (isEmpty ? 160 : stackHeight) + 48)
        contentContainer.frame = NSRect(x: 0, y: 0, width: bounds.width, height: totalHeight)
    }

    private var isEmpty: Bool {
        stackView.arrangedSubviews.isEmpty
    }
}

// MARK: - History Section Group View

final class HistorySectionView: NSView {
    var onNavigate: ((String) -> Void)?
    var onOpenNewTab: ((String) -> Void)?
    var onDelete: (() -> Void)?

    private let titleLabel = NSTextField(labelWithString: "")
    private let card = NSView()
    private let stack = NSStackView()

    init(sectionTitle: String, items: [HistoryItem]) {
        super.init(frame: .zero)
        wantsLayer = true

        titleLabel.stringValue = sectionTitle
        titleLabel.font = NSFont.systemFont(ofSize: 13, weight: .bold)
        titleLabel.textColor = Theme.chromeMutedText
        addSubview(titleLabel)

        card.wantsLayer = true
        card.layer?.cornerRadius = Theme.cornerRadius
        card.layer?.borderWidth = 1
        addSubview(card)

        stack.orientation = .vertical
        stack.spacing = 0
        stack.alignment = .leading
        card.addSubview(stack)

        for (index, item) in items.enumerated() {
            let row = HistoryPageRowView(item: item, isLast: index == items.count - 1)
            row.onNavigate = { [weak self] url in self?.onNavigate?(url) }
            row.onOpenNewTab = { [weak self] url in self?.onOpenNewTab?(url) }
            row.onDelete = { [weak self] in
                HistoryManager.shared.delete(id: item.id)
                self?.onDelete?()
            }
            stack.addArrangedSubview(row)
        }

        updateCardColors()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    private func updateCardColors() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        card.layer?.backgroundColor = Theme.cgColor(of: Theme.addressFieldBackground, for: effectiveAppearance)
        card.layer?.borderColor = Theme.cgColor(of: Theme.addressFieldBorder, for: effectiveAppearance)
        CATransaction.commit()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateCardColors()
    }

    override func layout() {
        super.layout()
        let w = bounds.width
        titleLabel.frame = NSRect(x: 4, y: 0, width: w - 8, height: 18)

        let cardY: CGFloat = 26
        let cardHeight = stack.fittingSize.height
        card.frame = NSRect(x: 0, y: cardY, width: w, height: cardHeight)
        stack.frame = NSRect(x: 0, y: 0, width: w, height: cardHeight)
    }

    override var intrinsicContentSize: NSSize {
        let stackH = stack.fittingSize.height
        return NSSize(width: NSView.noIntrinsicMetric, height: 26 + stackH)
    }
}

// MARK: - History Page Row View

final class HistoryPageRowView: NSView {
    let item: HistoryItem
    let isLast: Bool
    var onNavigate: ((String) -> Void)?
    var onOpenNewTab: ((String) -> Void)?
    var onDelete: (() -> Void)?

    private let timeLabel = NSTextField(labelWithString: "")
    private let monogram = MonogramView(radius: 8)
    private let titleLabel = NSTextField(labelWithString: "")
    private let urlLabel = NSTextField(labelWithString: "")

    private let openNewTabButton = GlyphButton(symbolName: "plus.square", glyphSize: 11)
    private let copyButton = GlyphButton(symbolName: "doc.on.doc", glyphSize: 11)
    private let deleteButton = GlyphButton(symbolName: "trash", glyphSize: 11)
    private let separator = HairlineView()

    private var isHovered = false

    init(item: HistoryItem, isLast: Bool) {
        self.item = item
        self.isLast = isLast
        super.init(frame: .zero)
        wantsLayer = true

        let formatter = DateFormatter()
        formatter.dateFormat = "h:mm a"
        timeLabel.stringValue = formatter.string(from: item.visitedAt)
        timeLabel.font = NSFont.systemFont(ofSize: 11, weight: .regular)
        timeLabel.textColor = Theme.chromeMutedText
        addSubview(timeLabel)

        monogram.configure(title: item.displayTitle)
        addSubview(monogram)

        titleLabel.stringValue = item.displayTitle
        titleLabel.font = Theme.fontButtonMedium
        titleLabel.textColor = Theme.chromeText
        titleLabel.lineBreakMode = .byTruncatingTail
        addSubview(titleLabel)

        urlLabel.stringValue = item.url
        urlLabel.font = Theme.fontCellURL
        urlLabel.textColor = Theme.chromeMutedText
        urlLabel.lineBreakMode = .byTruncatingTail
        addSubview(urlLabel)

        openNewTabButton.action = { [weak self, item] in self?.onOpenNewTab?(item.url) }
        addSubview(openNewTabButton)

        copyButton.action = { [item] in
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(item.url, forType: .string)
        }
        addSubview(copyButton)

        deleteButton.action = { [weak self] in self?.onDelete?() }
        addSubview(deleteButton)

        if !isLast {
            addSubview(separator)
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let h = bounds.height
        let w = bounds.width

        timeLabel.frame = NSRect(x: 14, y: (h - 16) / 2, width: 62, height: 16)
        let monoSide: CGFloat = 22
        monogram.frame = NSRect(x: 82, y: (h - monoSide) / 2, width: monoSide, height: monoSide)

        let textX: CGFloat = 114
        let actionWidth: CGFloat = 26 * 3 + 8
        let textWidth = max(60, w - textX - actionWidth - 16)

        titleLabel.frame = NSRect(x: textX, y: 9, width: textWidth, height: 17)
        urlLabel.frame = NSRect(x: textX, y: 28, width: textWidth, height: 15)

        var ax = w - 12 - 24
        deleteButton.frame = NSRect(x: ax, y: (h - 24) / 2, width: 24, height: 24)
        ax -= 26
        copyButton.frame = NSRect(x: ax, y: (h - 24) / 2, width: 24, height: 24)
        ax -= 26
        openNewTabButton.frame = NSRect(x: ax, y: (h - 24) / 2, width: 24, height: 24)

        if !isLast {
            separator.frame = NSRect(x: textX, y: h - Theme.hairlineThickness, width: w - textX, height: Theme.hairlineThickness)
        }
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: NSView.noIntrinsicMetric, height: 52)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(of: Theme.hoverFill, for: effectiveAppearance)
        CATransaction.commit()
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = NSColor.clear.cgColor
        CATransaction.commit()
    }

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        if !deleteButton.frame.contains(p) && !copyButton.frame.contains(p) && !openNewTabButton.frame.contains(p) {
            onNavigate?(item.url)
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
