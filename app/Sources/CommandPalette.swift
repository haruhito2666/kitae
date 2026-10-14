// CommandPalette.swift — the ⌘K switcher: a floating panel over dimmed
// content, filtering open tabs and quick actions. Arrow keys + Enter to
// run; Escape dismisses. Entrance scales/fades in under the motion gate.
import AppKit

// MARK: - Panel content

/// The palette's visual body: a soft rounded card with a search field on
/// top and the results table below a hairline. Owns the real controls —
/// the panel wires behavior into them.
final class PaletteContentView: NSView {
    let searchField = NSTextField()
    let tableView = NSTableView()
    let scrollView = NSScrollView()
    private let magnifier = NSTextField(labelWithString: "")
    private let hairline = HairlineView()

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = 14
        layer?.masksToBounds = true

        magnifier.stringValue = "⌕"
        magnifier.font = NSFont.systemFont(ofSize: 14, weight: .medium)
        magnifier.textColor = Theme.chromeMutedText

        searchField.font = Theme.fontPaletteTitle
        searchField.isBordered = false
        searchField.focusRingType = .none
        searchField.placeholderString = "Switch to a tab, run a command…"
        searchField.textColor = Theme.chromeText
        searchField.drawsBackground = false

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("main"))
        tableView.addTableColumn(column)
        tableView.headerView = nil
        tableView.backgroundColor = .clear
        tableView.rowSizeStyle = .default
        tableView.intercellSpacing = NSSize(width: 0, height: 0)
        tableView.selectionHighlightStyle = .regular

        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false

        addSubview(magnifier)
        addSubview(searchField)
        addSubview(hairline)
        addSubview(scrollView)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        needsLayout = true
    }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(of: Theme.cardBackground,
                                              for: effectiveAppearance)
        CATransaction.commit()

        magnifier.frame = NSRect(x: 16, y: 12, width: 16, height: 20)
        searchField.frame = NSRect(x: 38, y: 12, width: bounds.width - 52, height: 22)
        hairline.frame = NSRect(x: 0, y: 44, width: bounds.width, height: Theme.hairlineThickness)
        scrollView.frame = NSRect(x: 0, y: 44, width: bounds.width,
                                  height: max(0, bounds.height - 44))
        tableView.frame = NSRect(x: 0, y: 0,
                                 width: scrollView.bounds.width,
                                 height: tableView.frame.height)
    }
}

// MARK: - The panel

final class CommandPalette: NSPanel, NSTextFieldDelegate, NSTableViewDataSource,
    NSTableViewDelegate {

    enum ItemKind {
        case tab(index: Int)
        case action(() -> Void)
    }

    struct Item {
        let title: String
        let subtitle: String
        let kind: ItemKind
        var symbol: String?
    }

    private let content = PaletteContentView()
    private var allItems: [Item] = []
    private var visibleItems: [Item] = []
    private var isShown = false
    weak var hostWindow: NSWindow?
    var onSwitchToTab: ((Int) -> Void)?
    private var actions: [(String, () -> Void)] = []
    private var hostFade: DimmingView?

    convenience init() {
        self.init(contentRect: NSRect(x: 0, y: 0, width: 560, height: 300),
                  styleMask: [.borderless, .nonactivatingPanel],
                  backing: .buffered, defer: false)
        backgroundColor = .clear
        isOpaque = false
        hasShadow = true
        level = .modalPanel
        hidesOnDeactivate = false
        contentView = content

        content.searchField.delegate = self
        content.searchField.target = self
        content.searchField.action = #selector(runSelection)
        content.tableView.dataSource = self
        content.tableView.delegate = self
    }

    /// Borderless panels can't become key by default — without this the
    /// search field never takes focus and typing goes nowhere.
    override var canBecomeKey: Bool { true }

    // MARK: Item registry

    /// Quick-Look-style badge for file documents: tabs whose URL names a
    /// .qtsq document show the eye-in-square glyph instead of the site
    /// monogram — they're previewable files, not websites. nil keeps the
    /// monogram for ordinary URLs.
    private static func quickLookSymbol(forURL url: String) -> String? {
        url.lowercased().hasSuffix(".qtsq") ? "eye.square" : nil
    }

    func setSources(tabs: [(title: String, url: String, active: Bool)],
                    actions: [(String, () -> Void)]) {
        self.actions = actions
        allItems = tabs.enumerated().map { index, tab in
            Item(title: tab.title, subtitle: tab.url, kind: .tab(index: index),
                 symbol: Self.quickLookSymbol(forURL: tab.url))
        }
        let symbols = ["plus.circle", "arrow.clockwise", "chevron.left",
                       "chevron.right", "rectangle.topthird.inset.filled",
                       "sidebar.left", "gearshape"]
        allItems += actions.enumerated().map { index, action in
            Item(title: action.0, subtitle: "Command", kind: .action(action.1),
                 symbol: index < symbols.count ? symbols[index] : "sparkles")
        }
        rebuild(filter: content.searchField.stringValue)
    }

    // MARK: Show / hide

    func present(from window: NSWindow) {
        guard !isShown else { return }
        hostWindow = window
        isShown = true

        // Click-catcher over the host content: dismisses on outside click.
        if let hostContent = window.contentView {
            let dim = DimmingView(frame: hostContent.bounds)
            dim.onDismiss = { [weak self] in self?.dismiss() }
            dim.autoresizingMask = [.width, .height]
            hostContent.addSubview(dim, positioned: .above, relativeTo: nil)
            hostFade = dim
        }

        content.searchField.stringValue = ""
        rebuild(filter: "")
        centerOver(window)
        window.addChildWindow(self, ordered: .above)

        content.alphaValue = 0
        content.layer?.transform = CATransform3DMakeScale(0.96, 0.96, 1)
        makeKeyAndOrderFront(nil)
        if Theme.motionEnabled {
            Theme.animateEntrance {
                self.content.alphaValue = 1
                self.content.layer?.transform = CATransform3DIdentity
            }
        } else {
            content.alphaValue = 1
            content.layer?.transform = CATransform3DIdentity
        }
        makeFirstResponder(content.searchField)
    }

    func dismiss() {
        guard isShown else { return }
        isShown = false
        hostWindow?.removeChildWindow(self)
        orderOut(nil)
        hostFade?.removeFromSuperview()
        hostFade = nil
        hostWindow?.makeFirstResponder(nil)
    }

    override var isVisible: Bool { isShown }

    private func centerOver(_ window: NSWindow) {
        let host = window.frame
        let size = NSSize(width: 560, height: min(340, max(160, visibleHeight())))
        let x = host.midX - size.width / 2
        let y = host.midY - size.height / 2 + 40
        setFrame(NSRect(origin: NSPoint(x: x, y: y), size: size), display: true)
    }

    private func visibleHeight() -> CGFloat {
        44 + 12 + CGFloat(max(1, min(visibleItems.count, 7))) * 44
    }

    // MARK: Filtering

    private func rebuild(filter raw: String) {
        let query = raw.trimmingCharacters(in: .whitespaces).lowercased()
        if query.isEmpty {
            visibleItems = allItems
        } else {
            visibleItems = allItems.filter {
                $0.title.lowercased().contains(query)
                    || $0.subtitle.lowercased().contains(query)
            }
        }
        content.tableView.reloadData()
        if visibleItems.count > 0 {
            content.tableView.selectRowIndexes([0], byExtendingSelection: false)
            content.tableView.scrollRowToVisible(0)
        }
        if isShown {
            let size = NSSize(width: 560,
                              height: min(340, max(160, visibleHeight())))
            var frame = self.frame
            frame.origin.y += frame.height - size.height
            frame.size = size
            setFrame(frame, display: true)
        }
    }

    // MARK: NSTextFieldDelegate

    func controlTextDidChange(_ obj: Notification) {
        rebuild(filter: content.searchField.stringValue)
    }

    func control(_ control: NSControl, textView: NSTextView,
                 doCommandBy commandSelector: Selector) -> Bool {
        switch commandSelector {
        case #selector(NSResponder.moveUp(_:)):
            moveSelection(-1)
            return true
        case #selector(NSResponder.moveDown(_:)):
            moveSelection(1)
            return true
        case #selector(NSResponder.insertNewline(_:)):
            runSelection()
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            dismiss()
            return true
        default:
            return false
        }
    }

    @objc private func moveSelection(_ delta: Int) {
        guard !visibleItems.isEmpty else { return }
        let next = min(max(0, content.tableView.selectedRow + delta),
                       visibleItems.count - 1)
        content.tableView.selectRowIndexes([next], byExtendingSelection: false)
        content.tableView.scrollRowToVisible(next)
    }

    @objc private func runSelection() {
        let row = content.tableView.selectedRow
        guard visibleItems.indices.contains(row) else { return }
        switch visibleItems[row].kind {
        case .tab(let index):
            onSwitchToTab?(index)
            dismiss()
        case .action(let run):
            dismiss()
            run()
        }
    }

    // MARK: Table view

    func numberOfRows(in tableView: NSTableView) -> Int {
        visibleItems.count
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard visibleItems.indices.contains(row) else { return nil }
        let item = visibleItems[row]
        let id = NSUserInterfaceItemIdentifier("paletteCell")
        let cell: PaletteRowView
        if let reused = tableView.makeView(withIdentifier: id, owner: nil) as? PaletteRowView {
            cell = reused
        } else {
            cell = PaletteRowView()
            cell.identifier = id
        }
        var isAction = false
        var symbol: String?
        if case .action = item.kind { isAction = true }
        symbol = item.symbol
        cell.configure(title: item.title, subtitle: item.subtitle, isAction: isAction,
                       symbol: symbol)
        return cell
    }

    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat { 44 }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        PaletteRowHighlight()
    }
}

// MARK: - Rows & highlight

/// One result row: monogram, title, URL subtitle, and a "Switch to Tab"
/// hint with a return-key disc on the selected row.
final class PaletteRowView: NSView {
    private let monogram = MonogramView(radius: 9)
    private let iconView = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let subtitleLabel = NSTextField(labelWithString: "")
    private let hintLabel = NSTextField(labelWithString: "Switch to Tab")
    private let arrowDisc = NSTextField(labelWithString: "↩")
    private var isActionRow = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true

        iconView.symbolConfiguration = NSImage.SymbolConfiguration(
            pointSize: 14, weight: .medium)

        titleLabel.font = Theme.fontPaletteTitle
        titleLabel.lineBreakMode = .byTruncatingTail

        subtitleLabel.font = Theme.fontCellURL
        subtitleLabel.textColor = Theme.chromeMutedText
        subtitleLabel.lineBreakMode = .byTruncatingTail

        hintLabel.font = Theme.fontSectionLabel
        hintLabel.textColor = Theme.chromeMutedText
        hintLabel.isHidden = true

        arrowDisc.font = NSFont.systemFont(ofSize: 10, weight: .semibold)
        arrowDisc.alignment = .center
        arrowDisc.wantsLayer = true
        arrowDisc.layer?.cornerRadius = 9
        arrowDisc.layer?.masksToBounds = true
        arrowDisc.textColor = .white
        arrowDisc.isHidden = true

        addSubview(monogram)
        addSubview(iconView)
        addSubview(titleLabel)
        addSubview(subtitleLabel)
        addSubview(hintLabel)
        addSubview(arrowDisc)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    func configure(title: String, subtitle: String, isAction: Bool, symbol: String?) {
        isActionRow = isAction
        titleLabel.stringValue = title
        subtitleLabel.stringValue = isAction ? "Command" : subtitle
        if let symbol, let image = NSImage(systemSymbolName: symbol,
                                          accessibilityDescription: title) {
            image.isTemplate = true
            iconView.image = image
            iconView.contentTintColor = isAction ? Theme.accent : Theme.chromeMutedText
            iconView.isHidden = false
            monogram.isHidden = true
        } else {
            monogram.configure(title: subtitle.isEmpty ? title : subtitle)
            monogram.isHidden = false
            iconView.isHidden = true
        }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        arrowDisc.layer?.backgroundColor = Theme.cgColor(of: Theme.accent,
                                                        for: effectiveAppearance)
        CATransaction.commit()
    }

    /// Called by PaletteRowHighlight when the row's emphasis changes; shows
    /// the "Switch to Tab ↩" affordance on the selected row.
    func setHighlighted(_ value: Bool) {
        hintLabel.isHidden = !value
        arrowDisc.isHidden = !value
        needsLayout = true
    }

    override func layout() {
        super.layout()
        monogram.frame = NSRect(x: 14, y: (bounds.height - 22) / 2, width: 22, height: 22)
        iconView.frame = monogram.frame
        let textX = monogram.frame.maxX + 10
        let hintWidth: CGFloat = hintLabel.isHidden ? 0 : 92
        let textWidth = bounds.width - textX - hintWidth - 44
        // Title + subtitle as one optically centered block.
        let titleHeight = ceil(titleLabel.intrinsicContentSize.height)
        let subtitleHeight = ceil(subtitleLabel.intrinsicContentSize.height)
        let blockHeight = titleHeight + 2 + subtitleHeight
        let blockY = (bounds.height - blockHeight) / 2
        titleLabel.frame = NSRect(x: textX, y: blockY, width: textWidth, height: titleHeight)
        subtitleLabel.frame = NSRect(x: textX, y: blockY + titleHeight + 2,
                                     width: textWidth, height: subtitleHeight)
        hintLabel.frame = hintLabel.centeredTextFrame(
            x: bounds.width - hintWidth - 40, width: hintWidth, inHeight: bounds.height)
        arrowDisc.frame = NSRect(x: bounds.width - 32, y: (bounds.height - 18) / 2,
                                 width: 18, height: 18)
    }
}

/// Round row highlight so selection reads as a soft pill inside the card,
/// and the selected row's hint/arrow follow the row's emphasized state.
final class PaletteRowHighlight: NSTableRowView {
    override var isEmphasized: Bool {
        didSet { (viewOfType(PaletteRowView.self))?.setHighlighted(isEmphasized) }
    }

    private func viewOfType<T: NSView>(_ type: T.Type) -> T? {
        for sub in subviews { if let match = sub as? T { return match } }
        return nil
    }

    override func drawSelection(in dirtyRect: NSRect) {
        guard selectionHighlightStyle != .none else { return }
        let rect = bounds.insetBy(dx: 8, dy: 3)
        let path = NSBezierPath(roundedRect: rect, xRadius: 9, yRadius: 9)
        Theme.selectionFill.setFill()
        path.fill()
    }
}

/// Full-area click-catcher that dismisses the palette.
final class DimmingView: NSView {
    var onDismiss: (() -> Void)?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.withAlphaComponent(0.18).cgColor
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func mouseDown(with event: NSEvent) {
        onDismiss?()
    }

    override var acceptsFirstResponder: Bool { false }
}
