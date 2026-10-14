// ChromeView.swift — the app chrome around the page canvas.
//
// Two switchable layouts (inspired by modern Mac browsers, not copied):
//   * TopBar: a tab strip row above a toolbar row.
//   * Sidebar: a vertical tab rail with its own nav cluster and bottom bar.
//
// The page canvas always floats inside an inset rounded card over the
// backdrop — the "window within the window" that frames the web.
//
// Everything routes through shared, animated primitives so spacing, text
// and hover behavior stay consistent between the two layouts.

import AppKit

// MARK: - Address field (pill with magnifier + lock glyph)

// MARK: - Address field (pill with navigation + search box)

/// Text field cell that vertically centers single-line text within any frame height.
final class VerticallyCenteredTextFieldCell: NSTextFieldCell {
    override func drawingRect(forBounds rect: NSRect) -> NSRect {
        let textRect = super.drawingRect(forBounds: rect)
        let textSize = cellSize(forBounds: rect)
        let dy = round((rect.height - textSize.height) / 2)
        return NSRect(x: textRect.origin.x, y: rect.origin.y + dy, width: textRect.width, height: textSize.height)
    }

    override func select(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, start selStart: Int, length selLength: Int) {
        let textRect = drawingRect(forBounds: rect)
        super.select(withFrame: textRect, in: controlView, editor: textObj, delegate: delegate, start: selStart, length: selLength)
    }

    override func edit(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, event: NSEvent?) {
        let textRect = drawingRect(forBounds: rect)
        super.edit(withFrame: textRect, in: controlView, editor: textObj, delegate: delegate, event: event)
    }
}

final class AddressField: NSTextField, NSTextFieldDelegate {
    var onSubmit: ((String) -> Void)?
    var alwaysShowFullURL = SettingsStore.shared.alwaysShowFullURL
    var highlightTopLevelDomain = SettingsStore.shared.highlightTopLevelDomain
    private let placeholderBase: String
    private var currentURL = ""
    /// True once the user has typed into the focused field. An auto-created
    /// field editor (AppKit hands the search field initial key focus) must
    /// not block page-driven URL sync — real keystrokes should.
    private var userEdited = false

    init(placeholder: String) {
        placeholderBase = placeholder
        super.init(frame: .zero)
        let centeredCell = VerticallyCenteredTextFieldCell(textCell: "")
        centeredCell.isEditable = true
        centeredCell.isSelectable = true
        cell = centeredCell
        isEditable = true
        isSelectable = true
        placeholderString = placeholder
        font = Theme.fontAddress
        alignment = .center
        isBordered = false
        isBezeled = false
        focusRingType = .none
        delegate = self
        target = self
        action = #selector(submit)
        drawsBackground = false
        textColor = Theme.chromeText
        lineBreakMode = .byTruncatingTail
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func submit() {
        let value = stringValue
        onSubmit?(value)
        userEdited = false
        window?.makeFirstResponder(superview)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            submit()
            return true
        } else if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            window?.makeFirstResponder(superview)
            return true
        }
        return false
    }

    override func textDidChange(_ notification: Notification) {
        userEdited = true
        super.textDidChange(notification)
    }

    override func mouseDown(with event: NSEvent) {
        let isAlreadyEditing = (window?.firstResponder === currentEditor())
        super.mouseDown(with: event)
        if !isAlreadyEditing {
            currentEditor()?.selectAll(nil)
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .iBeam)
    }

    override func becomeFirstResponder() -> Bool {
        let ok = super.becomeFirstResponder()
        if ok { currentEditor()?.selectAll(nil) }  // ⌘L → edit the whole address
        return ok
    }

    /// Keep the field in sync with page navigation unless the user is
    /// mid-edit (keystrokes present in the field editor).
    func sync(url: String) {
        currentURL = url
        if currentEditor() != nil && userEdited { return }
        applyDisplayValue(displayValue(for: url))
    }

    func refreshURL() {
        if currentEditor() != nil && userEdited { return }
        applyDisplayValue(displayValue(for: currentURL))
    }

    private func applyDisplayValue(_ value: String) {
        guard highlightTopLevelDomain else {
            stringValue = value
            return
        }
        var domainBuffer = [CChar](repeating: 0, count: 512)
        let domainLength = value.withCString {
            kitae_registrable_domain($0, &domainBuffer, domainBuffer.count)
        }
        let domain = String(cString: domainBuffer)
        guard domainLength > 1,
              let range = value.range(of: domain) else {
            stringValue = value
            return
        }
        let attributed = NSMutableAttributedString(
            string: value,
            attributes: [.font: Theme.fontAddress, .foregroundColor: Theme.chromeText]
        )
        attributed.addAttributes(
            [.foregroundColor: Theme.accent],
            range: NSRange(range, in: value)
        )
        attributedStringValue = attributed
    }

    private func displayValue(for url: String) -> String {
        guard !alwaysShowFullURL,
              let parsed = URL(string: url),
              let host = parsed.host,
              !host.isEmpty else { return url }
        var value = host
        if !parsed.path.isEmpty { value += parsed.path }
        if let query = parsed.query { value += "?\(query)" }
        if let fragment = parsed.fragment { value += "#\(fragment)" }
        return value
    }
}

/// Container for the search text field that routes clicks anywhere on it to focus the field.
final class SearchFieldContainerView: NSView {
    var onMouseDown: (() -> Void)?
    override var isFlipped: Bool { true }
    override func mouseDown(with event: NSEvent) {
        onMouseDown?()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .iBeam)
    }
}

/// The rounded address pill: houses the navigation controls (back, forward, reload)
/// on the left inside the box, the search text field inside its own grey box,
/// and symmetric spacing on the right.
final class AddressPillView: NSView {
    let backButton = GlyphButton(symbolName: "chevron.left", glyphSize: 16)
    let forwardButton = GlyphButton(symbolName: "chevron.right", glyphSize: 16)
    let reloadButton = LottieReloadButton()
    let searchContainer = SearchFieldContainerView()
    let field: AddressField
    let shareButton = GlyphButton(image: .linkIcon, glyphSize: 16)
    let adjustmentsButton = GlyphButton(image: .adjustmentsIcon, glyphSize: 16)
    private let securityIcon = LottieIconView()
    private var isLoading = false
    private var currentURL = ""
    private var focusTracking = false
    private var isFocused = false
    var onOpenSettings: (() -> Void)?

    init(placeholder: String) {
        field = AddressField(placeholder: placeholder)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = Theme.addressFieldHeight / 2
        layer?.masksToBounds = false

        searchContainer.wantsLayer = true
        searchContainer.layer?.cornerRadius = 8
        searchContainer.layer?.masksToBounds = true
        searchContainer.onMouseDown = { [weak self] in
            guard let self = self else { return }
            self.window?.makeFirstResponder(self.field)
            self.field.currentEditor()?.selectAll(nil)
        }

        adjustmentsButton.action = { [weak self] in
            self?.togglePageInfo()
        }

        searchContainer.addSubview(adjustmentsButton)
        searchContainer.addSubview(securityIcon)
        searchContainer.addSubview(field)

        addSubview(backButton)
        addSubview(forwardButton)
        addSubview(reloadButton)
        addSubview(searchContainer)
        addSubview(shareButton)

        NotificationCenter.default.addObserver(
            self, selector: #selector(focusChanged),
            name: NSWindow.didBecomeKeyNotification, object: nil)
        NotificationCenter.default.addObserver(
            self, selector: #selector(focusChanged),
            name: NSWindow.didUpdateNotification, object: nil)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func focusChanged() {
        let focused = window?.firstResponder is NSText && (window?.firstResponder as? NSText)?.delegate === field
        setFocused(focused)
    }

    func setFocused(_ value: Bool) {
        guard value != isFocused else { return }
        isFocused = value
        Theme.animate { refresh() }
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setAnimationDuration(Theme.motionEnabled ? Theme.animationDuration : 0)
        layer?.backgroundColor = Theme.cgColor(of: Theme.addressFieldBackground,
                                              for: effectiveAppearance)
        layer?.borderWidth = 1
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        // Always neutral — no accent outline on focus
        let borderCol = isDark ? Theme.addressFieldBorder : NSColor(white: 1.0, alpha: 0.70)
        layer?.borderColor = Theme.cgColor(of: borderCol, for: effectiveAppearance)

        layer?.shadowColor = NSColor.black.cgColor
        layer?.shadowOpacity = isFocused ? 0.12 : 0.06
        layer?.shadowOffset = CGSize(width: 0, height: -2)
        layer?.shadowRadius = isFocused ? 10 : 5
        if bounds.width > 0 && bounds.height > 0 {
            layer?.shadowPath = CGPath(roundedRect: bounds,
                                       cornerWidth: bounds.height / 2,
                                       cornerHeight: bounds.height / 2,
                                       transform: nil)
        }

        // Inner search box styling — always neutral, never accent-coloured
        let searchBg = Theme.dynamic(
            NSColor(white: 0.0, alpha: 0.055),
            NSColor(white: 1.0, alpha: 0.08)
        )
        let searchBorder = Theme.dynamic(
            NSColor(white: 0.0, alpha: 0.04),
            NSColor(white: 1.0, alpha: 0.05)
        )
        searchContainer.layer?.backgroundColor = Theme.cgColor(
            of: isFocused ? Theme.dynamic(NSColor(white: 0.0, alpha: 0.075), NSColor(white: 1.0, alpha: 0.12)) : searchBg,
            for: effectiveAppearance)
        searchContainer.layer?.borderWidth = 0.5
        searchContainer.layer?.borderColor = Theme.cgColor(of: searchBorder, for: effectiveAppearance)

        CATransaction.commit()
        updateSecurityState()
        needsDisplay = true
    }

    func togglePageInfo() {
        let usesHTTPS = currentURL.hasPrefix("https://") || kitae_hsts_covers(currentURL) != 0
        PageInfoPopover.shared.toggle(
            relativeTo: adjustmentsButton,
            url: currentURL,
            usesHTTPS: usesHTTPS,
            onOpenSettings: { [weak self] in
                self?.onOpenSettings?()
            }
        )
    }

    func setLoading(_ loading: Bool) {
        guard loading != isLoading else { return }
        isLoading = loading
        reloadButton.setLoading(loading)
        updateSecurityState()
    }

    func syncURL(_ url: String) {
        currentURL = url
        updateSecurityState()
    }

    private func updateSecurityState() {
        let url = currentURL.isEmpty ? field.stringValue : currentURL
        if url.isEmpty || url == "about:start" {
            securityIcon.setState(.hidden)
            return
        }
        if isLoading {
            securityIcon.setState(.loading)
        } else {
            if url.hasPrefix("https://") || kitae_hsts_covers(url) != 0 {
                securityIcon.setState(.secure)
            } else if url.hasPrefix("http://") {
                securityIcon.setState(.insecure)
            } else {
                securityIcon.setState(.hidden)
            }
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let width = bounds.width
        let height = bounds.height

        // Left nav buttons: 30×30pt with 4pt gaps, starting 10pt from left edge
        let navBtnSide: CGFloat = 30
        let leftEdgePad: CGFloat = 10
        let btnGap: CGFloat = 4
        let navY = (height - navBtnSide) / 2

        backButton.frame    = NSRect(x: leftEdgePad,                                   y: navY, width: navBtnSide, height: navBtnSide)
        forwardButton.frame = NSRect(x: leftEdgePad + navBtnSide + btnGap,            y: navY, width: navBtnSide, height: navBtnSide)
        reloadButton.frame  = NSRect(x: leftEdgePad + (navBtnSide + btnGap) * 2,      y: navY, width: navBtnSide, height: navBtnSide)

        // Share button sits inside the pill to the right of searchContainer
        let shareSide: CGFloat = 30
        let shareGap: CGFloat  = 8   // gap between searchContainer right edge and share button
        let rightPad: CGFloat  = 10  // gap between share button right edge and pill right edge

        // Left spacing mirrors the nav buttons area
        let leftSpacing: CGFloat = reloadButton.frame.maxX + 10   // ≈ 118 pt
        // Right spacing = gap + shareButton + rightPad
        let rightSpacing: CGFloat = shareGap + shareSide + rightPad   // = 48 pt

        let boxW = max(120, width - leftSpacing - rightSpacing)
        let boxH: CGFloat = 30
        let boxY = (height - boxH) / 2

        searchContainer.frame = NSRect(x: leftSpacing, y: boxY, width: boxW, height: boxH)

        // Share button: just to the right of searchContainer, vertically centered
        shareButton.frame = NSRect(x: searchContainer.frame.maxX + shareGap,
                                   y: (height - shareSide) / 2,
                                   width: shareSide, height: shareSide)

        // Adjustments button (SVG Tabler Adjustments Horizontal): 24×24pt with 16pt glyph, vertically centered, 4pt from left edge
        let adjSide: CGFloat = 24
        adjustmentsButton.frame = NSRect(x: 4, y: (boxH - adjSide) / 2, width: adjSide, height: adjSide)

        // Security icon (Lottie animation): 20×20pt, vertically centered, directly next to adjustments button
        let iconSide: CGFloat = 20
        securityIcon.frame = NSRect(x: adjustmentsButton.frame.maxX + 4, y: (boxH - iconSide) / 2, width: iconSide, height: iconSide)

        // Text field occupies searchContainer with symmetric margins so centered text is dead center
        let sidePad: CGFloat = 54
        field.frame = NSRect(x: sidePad, y: 0, width: max(40, boxW - sidePad * 2), height: boxH)

        refresh()
    }

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            window?.zoom(nil)
        } else {
            window?.performDrag(with: event)
        }
    }
}


// MARK: - Toolbar

final class ToolbarView: NSView {
    let addressPill: AddressPillView
    var addressField: AddressField { addressPill.field }
    var backButton: GlyphButton { addressPill.backButton }
    var forwardButton: GlyphButton { addressPill.forwardButton }
    var reloadButton: LottieReloadButton { addressPill.reloadButton }
    // shareButton now lives inside the pill; expose via passthrough so AppDelegate wiring unchanged
    var shareButton: GlyphButton { addressPill.shareButton }
    var onOpenSettings: (() -> Void)? {
        get { addressPill.onOpenSettings }
        set { addressPill.onOpenSettings = newValue }
    }
    // toolbar sidebarButton: shown when sidebar is COLLAPSED → clicking expands it
    let sidebarButton = GlyphButton(image: .sidebarExpandIcon, glyphSize: 24)
    let menuButton = LottieMenuButton()
    // H1: opens the Downloads popover; badge shows the active-download count.
    let downloadsButton = GlyphButton(symbolName: "arrow.down.circle", glyphSize: 16)
    private let downloadsBadge = NSTextField(labelWithString: "")

    var showsNavButtons = true {
        didSet { needsLayout = true }
    }

    var showsSidebarButton = false {
        didSet {
            sidebarButton.isHidden = !showsSidebarButton
            needsLayout = true
        }
    }

    /// Every glyph button used for repaint nudges on mode switches.
    var buttons: [GlyphButton] { [sidebarButton, addressPill.shareButton, downloadsButton] }

    init(addressPlaceholder: String) {
        addressPill = AddressPillView(placeholder: addressPlaceholder)
        super.init(frame: .zero)

        sidebarButton.isHidden = true
        addSubview(sidebarButton)
        addSubview(addressPill)
        addSubview(menuButton)
        addSubview(downloadsButton)
        addSubview(downloadsBadge)
    }

    /// H1: badge the downloads button with the active count (hidden at 0).
    func setDownloadBadge(count: Int) {
        if count > 0 {
            downloadsBadge.stringValue = count > 9 ? "9+" : "\(count)"
            downloadsBadge.font = NSFont.systemFont(ofSize: 9, weight: .bold)
            downloadsBadge.alignment = .center
            downloadsBadge.textColor = .white
            downloadsBadge.wantsLayer = true
            downloadsBadge.layer?.backgroundColor = Theme.accent.cgColor
            downloadsBadge.layer?.cornerRadius = 7
            downloadsBadge.layer?.masksToBounds = true
        }
        downloadsBadge.isHidden = count == 0
        needsLayout = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let h = bounds.height
        let buttonSide: CGFloat = 30
        let rightMargin: CGFloat = 14

        // Trailing: downloads button + menu button
        let menuX = bounds.width - rightMargin - buttonSide
        menuButton.frame = NSRect(x: menuX, y: (h - buttonSide) / 2,
                                  width: buttonSide, height: buttonSide)
        let dlGap: CGFloat = 2
        downloadsButton.frame = NSRect(x: menuX - buttonSide - dlGap, y: (h - buttonSide) / 2,
                                       width: buttonSide, height: buttonSide)
        let badgeSide: CGFloat = 14
        downloadsBadge.frame = NSRect(x: downloadsButton.frame.maxX - badgeSide + 3,
                                      y: (h - badgeSide) / 2 - 8,
                                      width: badgeSide, height: badgeSide)
        downloadsBadge.layer?.cornerRadius = badgeSide / 2

        // Leading sidebar button (shown when sidebar is collapsed)
        var pillLeading: CGFloat = leadingInset + 10
        if showsSidebarButton {
            let sideBtnX = leadingInset + 4
            sidebarButton.frame = NSRect(x: sideBtnX, y: (h - buttonSide) / 2,
                                         width: buttonSide, height: buttonSide)
            pillLeading = sidebarButton.frame.maxX + 10
        }

        // Address pill: stretches from leading inset to just before the downloads button
        let boxRightSpacing: CGFloat = 14
        let pillWidth = max(120, downloadsButton.frame.minX - boxRightSpacing - pillLeading)
        let pillHeight = Theme.addressFieldHeight

        addressPill.frame = NSRect(x: pillLeading,
                                   y: (h - pillHeight) / 2,
                                   width: pillWidth,
                                   height: pillHeight)
    }

    /// Left inset for traffic lights when this toolbar is the topmost row.
    var leadingInset: CGFloat = 0

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            window?.zoom(nil)
        } else {
            window?.performDrag(with: event)
        }
    }
}


// MARK: - Loading hairline

/// The 2 pt indeterminate progress strip under the toolbar.
final class LoadingBarView: NSView {
    private let strip = NSView(frame: .zero)
    private var stripLayer: CALayer? {
        strip.layer
    }
    private var running = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        clipsToBounds = true
        strip.wantsLayer = true
        addSubview(strip)
        isHidden = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let height = bounds.height
        let stripWidth = bounds.width * 0.28
        strip.frame = NSRect(x: -stripWidth, y: 0, width: stripWidth, height: height)
        if let layer = stripLayer {
            layer.backgroundColor = Theme.loadingGradient[1].cgColor
            layer.cornerRadius = height / 2
        }
    }

    func setLoading(_ loading: Bool) {
        let wasHidden = isHidden
        // Fade the bar in/out instead of popping it.
        Theme.animate { isHidden = !loading }
        if loading {
            startAnimation()
        } else if !wasHidden {
            stopAnimation()
        }
        needsLayout = true
    }

    private func startAnimation() {
        guard !running else { return }
        running = true
        guard let layer = stripLayer, Theme.motionEnabled else {
            // Reduce Motion: a static accent hairline.
            strip.frame = NSRect(x: 0, y: 0, width: bounds.width, height: bounds.height)
            return
        }
        let travel = CABasicAnimation(keyPath: "position.x")
        travel.fromValue = -(bounds.width * 0.28) / 2
        travel.toValue = bounds.width + (bounds.width * 0.28) / 2
        travel.duration = 1.1
        travel.repeatCount = .infinity
        travel.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        layer.add(travel, forKey: "sweep")
    }

    private func stopAnimation() {
        running = false
        stripLayer?.removeAnimation(forKey: "sweep")
    }
}

// MARK: - Tab strip (top-bar layout)

final class TabStripView: NSView {
    var onSelect: ((Int) -> Void)?
    var onClose: ((Int) -> Void)?
    var onNewTab: (() -> Void)?

    private let newTabButton = GlyphButton(symbolName: "plus")
    private var cells: [TabCellView] = []
    private var showCloseButtonsOnInactiveTabs = false
    private let insetForTrafficLights: CGFloat = 84

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        addSubview(newTabButton)
        newTabButton.action = { [weak self] in self?.onNewTab?() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    func reload(tabs: [Tab], activeIndex: Int, showCloseButtonsOnInactiveTabs: Bool,
                usePageColor: Bool) {
        self.showCloseButtonsOnInactiveTabs = showCloseButtonsOnInactiveTabs
        for cell in cells { cell.removeFromSuperview() }
        cells = tabs.enumerated().map { index, tab in
            let cell = TabCellView(index: index)
            let frame = tab.latestFrame
            let pageColor = usePageColor && frame?.opaquePageBackground == true
                ? EngineFrame.color(frame!.pageBackground)
                : nil
            cell.configure(title: tabTitle(tab), url: tab.latestFrame?.url ?? "",
                           active: index == activeIndex,
                           showCloseButtonOnInactiveTab: showCloseButtonsOnInactiveTabs,
                           pageColor: pageColor)
            cell.onSelect = { [weak self] in self?.onSelect?(index) }
            cell.onClose = { [weak self] in self?.onClose?(index) }
            addSubview(cell)
            return cell
        }
        needsLayout = true
    }

    private func tabTitle(_ tab: Tab) -> String {
        if let frame = tab.latestFrame, !frame.title.isEmpty { return frame.title }
        return "New Tab"
    }

    override func layout() {
        super.layout()
        let side: CGFloat = 24
        newTabButton.frame = NSRect(x: bounds.width - side - Theme.paddingMedium,
                                    y: (bounds.height - side) / 2,
                                    width: side, height: side)

        let count = cells.count
        guard count > 0 else { return }
        let available = bounds.width - insetForTrafficLights - side - Theme.paddingMedium * 2
        let gap: CGFloat = 6
        let width = min(Theme.tabWidth, max(120, (available - gap * CGFloat(count - 1)) / CGFloat(count)))

        var x = insetForTrafficLights
        for cell in cells {
            cell.frame = NSRect(x: x, y: (bounds.height - Theme.tabHeight) / 2,
                                width: width, height: Theme.tabHeight)
            x += width + gap
        }
    }

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            window?.zoom(nil)
        } else {
            window?.performDrag(with: event)
        }
    }
}

/// One pill-shaped tab in the strip: monogram + title, close on hover.
final class TabCellView: NSView {
    let index: Int
    var onSelect: (() -> Void)?
    var onClose: (() -> Void)?

    private let monogram = MonogramView(radius: 6.5)
    private let titleLabel = NSTextField(labelWithString: "")
    private let closeButton = GlyphButton(symbolName: "xmark", glyphSize: 8) {}
    private var isActive = false
    private var isHovered = false
    private var mouseInside = false
    private var showCloseButtonOnInactiveTab = false
    private var pageColor: NSColor?
    private var cellTitle = ""

    init(index: Int) {
        self.index = index
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        wantsLayer = true
        layer?.cornerRadius = Theme.cornerRadius
        layer?.masksToBounds = false

        titleLabel.font = Theme.fontTabLabel
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.cell?.truncatesLastVisibleLine = true
        addSubview(monogram)
        addSubview(titleLabel)

        closeButton.action = { [weak self] in self?.onClose?() }
        addSubview(closeButton)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override var acceptsFirstResponder: Bool { true }

    override func accessibilityPerformPress() -> Bool {
        onSelect?()
        return true
    }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 76 || event.keyCode == 49 {
            _ = accessibilityPerformPress()
        } else {
            super.keyDown(with: event)
        }
    }

    func configure(title: String, url: String, active: Bool,
                   showCloseButtonOnInactiveTab: Bool, pageColor: NSColor?) {
        cellTitle = title
        titleLabel.stringValue = title
        setAccessibilityLabel(title)
        setAccessibilityValue(NSNumber(value: active))
        monogram.configure(title: url.isEmpty ? title : url)
        isActive = active
        self.showCloseButtonOnInactiveTab = showCloseButtonOnInactiveTab
        self.pageColor = pageColor
        updateColors()
    }

    private func updateColors() {
        let background: NSColor
        if isActive {
            background = pageColor?.blended(withFraction: 0.12, of: Theme.tabSelected)
                ?? Theme.tabSelected
        } else if isHovered {
            background = Theme.hoverFill
        } else {
            background = .clear
        }
        CATransaction.begin()
        CATransaction.setAnimationDuration(Theme.motionEnabled ? Theme.animationDuration : 0)
        layer?.backgroundColor = Theme.cgColor(of: background, for: effectiveAppearance)

        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        if isActive {
            layer?.borderWidth = 1
            layer?.borderColor = isDark
                ? NSColor(white: 1.0, alpha: 0.15).cgColor
                : NSColor(white: 1.0, alpha: 0.70).cgColor
            layer?.shadowColor = NSColor.black.cgColor
            layer?.shadowOpacity = isDark ? 0.20 : 0.07
            layer?.shadowOffset = CGSize(width: 0, height: -2)
            layer?.shadowRadius = 8
        } else {
            layer?.borderWidth = 0
            layer?.shadowOpacity = 0
        }
        CATransaction.commit()
        titleLabel.textColor = isActive ? Theme.chromeText : Theme.chromeMutedText
        closeButton.isHidden = !(isActive || showCloseButtonOnInactiveTab || mouseInside)
        needsDisplay = true
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateColors()
    }

    override func layout() {
        super.layout()
        let closeSide: CGFloat = 18
        closeButton.frame = NSRect(x: bounds.width - closeSide - 4,
                                   y: (bounds.height - closeSide) / 2,
                                   width: closeSide, height: closeSide)
        let monoSide: CGFloat = 15
        monogram.frame = NSRect(x: 8, y: (bounds.height - monoSide) / 2,
                                width: monoSide, height: monoSide)
        let textX = monogram.frame.maxX + 6
        titleLabel.frame = titleLabel.centeredTextFrame(
            x: textX, width: closeButton.frame.minX - textX - 2,
            inHeight: bounds.height)

        if bounds.width > 0 && bounds.height > 0 && isActive {
            layer?.shadowPath = CGPath(roundedRect: bounds,
                                       cornerWidth: Theme.cornerRadius,
                                       cornerHeight: Theme.cornerRadius,
                                       transform: nil)
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        mouseInside = true
        updateColors()
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        mouseInside = false
        updateColors()
    }

    override func mouseDown(with event: NSEvent) {
        onSelect?()
    }

    override func draw(_ dirtyRect: NSRect) {
        // Background handled by the layer; nothing per-frame here.
    }
}

// MARK: - Sidebar (sidebar layout)

/// The search field in the sidebar used to search and filter open tabs.
final class SidebarTabSearchBar: NSView {
    var onQueryChange: ((String) -> Void)?

    private let searchIcon = NSImageView()
    let field = NSTextField()
    private let clearButton = GlyphButton(symbolName: "xmark.circle.fill", glyphSize: 11) {}
    private var isFocused = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = 9
        layer?.masksToBounds = false

        let config = NSImage.SymbolConfiguration(pointSize: 11, weight: .medium)
        searchIcon.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)?
            .withSymbolConfiguration(config)
        searchIcon.imageScaling = .scaleProportionallyUpOrDown
        searchIcon.contentTintColor = Theme.chromeMutedText
        addSubview(searchIcon)

        field.isBordered = false
        field.isBezeled = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = NSFont.systemFont(ofSize: 13, weight: .regular)
        field.textColor = Theme.chromeText
        field.placeholderString = "Search tabs"
        field.target = self
        field.action = #selector(textChanged)
        addSubview(field)

        clearButton.action = { [weak self] in
            self?.field.stringValue = ""
            self?.textChanged()
        }
        clearButton.isHidden = true
        addSubview(clearButton)

        NotificationCenter.default.addObserver(
            self, selector: #selector(fieldDidChange),
            name: NSControl.textDidChangeNotification, object: field)
        NotificationCenter.default.addObserver(
            self, selector: #selector(focusChanged),
            name: NSWindow.didBecomeKeyNotification, object: nil)
        NotificationCenter.default.addObserver(
            self, selector: #selector(focusChanged),
            name: NSWindow.didUpdateNotification, object: nil)

        refresh()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    @objc private func textChanged() {
        let query = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        clearButton.isHidden = query.isEmpty
        onQueryChange?(query)
    }

    @objc private func fieldDidChange() {
        textChanged()
    }

    @objc private func focusChanged() {
        let focused = window?.firstResponder is NSText && (window?.firstResponder as? NSText)?.delegate === field
        guard focused != isFocused else { return }
        isFocused = focused
        Theme.animate { refresh() }
    }

    private func refresh() {
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        // No accent outline on focus — the container handles its own visual style
        layer?.backgroundColor = isDark
            ? NSColor(white: isFocused ? 0.22 : 0.20, alpha: isFocused ? 0.95 : 0.88).cgColor
            : NSColor(white: 1.0, alpha: isFocused ? 0.96 : 0.88).cgColor
        layer?.borderWidth = isFocused ? 1.0 : 0.75
        layer?.borderColor = isDark
            ? NSColor(white: 1.0, alpha: isFocused ? 0.18 : 0.12).cgColor
            : NSColor(white: 0.0, alpha: isFocused ? 0.12 : 0.08).cgColor
        layer?.shadowColor = NSColor.black.cgColor
        layer?.shadowOpacity = isFocused ? 0.07 : 0.04
        layer?.shadowOffset = CGSize(width: 0, height: 1)
        layer?.shadowRadius = isFocused ? 4 : 3
        CATransaction.commit()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    override func layout() {
        super.layout()
        layer?.cornerRadius = 9
        let h = bounds.height
        let w = bounds.width

        let iconSide: CGFloat = 12
        searchIcon.frame = NSRect(x: 10, y: (h - iconSide) / 2, width: iconSide, height: iconSide)

        let clearSide: CGFloat = 16
        clearButton.frame = NSRect(x: w - clearSide - 8, y: (h - clearSide) / 2, width: clearSide, height: clearSide)

        let fieldX = searchIcon.frame.maxX + 8
        let fieldRight = clearButton.isHidden ? (w - 10) : clearButton.frame.minX - 4
        let fieldW = max(20, fieldRight - fieldX)
        let fieldH: CGFloat = 18
        field.frame = NSRect(x: fieldX, y: (h - fieldH) / 2, width: fieldW, height: fieldH)

        refresh()
    }
}

final class SidebarView: NSView {
    var onSelect: ((Int) -> Void)?
    var onClose: ((Int) -> Void)?
    var onNewTab: (() -> Void)?
    var onToggle: (() -> Void)?

    // sidebar toggleButton: shown when sidebar IS visible → clicking collapses it
    let toggleButton = GlyphButton(image: .sidebarCollapseIcon, glyphSize: 24)
    let searchBar = SidebarTabSearchBar()
    let separator = NSView()
    private let newTabRow = NewTabRow()
    private let scrollView = NSScrollView()
    private let stackView = NSStackView()
    private var cells: [SidebarCellView] = []

    private var cachedTabs: [Tab] = []
    private var cachedActiveIndex: Int = 0
    private var filterQuery: String = ""
    private var showCloseButtonsOnInactiveTabs = false
    private var usePageColor = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.clear.cgColor

        toggleButton.action = { [weak self] in self?.onToggle?() }
        addSubview(toggleButton)

        searchBar.onQueryChange = { [weak self] query in
            self?.filterQuery = query
            self?.applyFilter(showCloseButtonsOnInactiveTabs: self?.showCloseButtonsOnInactiveTabs ?? false,
                             usePageColor: self?.usePageColor ?? false)
        }
        addSubview(searchBar)

        separator.wantsLayer = true
        separator.layer?.backgroundColor = Theme.dynamic(
            NSColor(white: 0.0, alpha: 0.10),
            NSColor(white: 1.0, alpha: 0.08)
        ).cgColor
        addSubview(separator)

        newTabRow.action = { [weak self] in self?.onNewTab?() }
        addSubview(newTabRow)

        scrollView.hasVerticalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        addSubview(scrollView)

        stackView.orientation = .vertical
        stackView.spacing = 4
        stackView.alignment = .centerX
        stackView.edgeInsets = NSEdgeInsets(top: 2, left: 12, bottom: 12, right: 12)
        scrollView.documentView = stackView
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    func reload(tabs: [Tab], activeIndex: Int, showCloseButtonsOnInactiveTabs: Bool,
                usePageColor: Bool) {
        cachedTabs = tabs
        cachedActiveIndex = activeIndex
        applyFilter(showCloseButtonsOnInactiveTabs: showCloseButtonsOnInactiveTabs,
                    usePageColor: usePageColor)
    }

    private func applyFilter(showCloseButtonsOnInactiveTabs: Bool, usePageColor: Bool) {
        self.showCloseButtonsOnInactiveTabs = showCloseButtonsOnInactiveTabs
        self.usePageColor = usePageColor
        for cell in cells { cell.removeFromSuperview() }
        cells.removeAll()

        let filtered = cachedTabs.enumerated().filter { index, tab in
            if filterQuery.isEmpty { return true }
            let q = filterQuery.lowercased()
            let title = tabTitle(tab).lowercased()
            let url = (tab.latestFrame?.url ?? "").lowercased()
            return title.contains(q) || url.contains(q)
        }

        for (index, tab) in filtered {
            let cell = SidebarCellView(index: index)
            let frame = tab.latestFrame
            let pageColor = usePageColor && frame?.opaquePageBackground == true
                ? EngineFrame.color(frame!.pageBackground)
                : nil
            cell.configure(title: tabTitle(tab),
                           url: tab.latestFrame?.url ?? "",
                           active: index == cachedActiveIndex,
                           showCloseButtonOnInactiveTab: showCloseButtonsOnInactiveTabs,
                           pageColor: pageColor)
            cell.onSelect = { [weak self] in self?.onSelect?(index) }
            cell.onClose = { [weak self] in self?.onClose?(index) }
            stackView.addArrangedSubview(cell)
            cell.translatesAutoresizingMaskIntoConstraints = false
            cell.widthAnchor.constraint(equalTo: stackView.widthAnchor, constant: -24).isActive = true
            cell.heightAnchor.constraint(equalToConstant: 38).isActive = true
            cells.append(cell)
        }
    }

    private func tabTitle(_ tab: Tab) -> String {
        if let frame = tab.latestFrame, !frame.title.isEmpty { return frame.title }
        return "New Tab"
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        layer?.backgroundColor = NSColor.clear.cgColor
        separator.layer?.backgroundColor = Theme.dynamic(
            NSColor(white: 0.0, alpha: 0.10),
            NSColor(white: 1.0, alpha: 0.08)
        ).cgColor
    }

    override func layout() {
        super.layout()
        let width = bounds.width

        // Top row: traffic lights sit on the left (x: ~16, y: ~18);
        // Sidebar toggle button placed at the top-right corner of the sidebar:
        let btnSide: CGFloat = 30
        let topBtnY: CGFloat = 13
        toggleButton.frame = NSRect(x: width - 14 - btnSide, y: topBtnY, width: btnSide, height: btnSide)

        // Search tabs bar directly below top row
        let searchY: CGFloat = 48
        let searchH: CGFloat = 34
        searchBar.frame = NSRect(x: 12, y: searchY, width: width - 24, height: searchH)

        // Subtle separator line below search bar
        let sepY = searchY + searchH + 10
        separator.frame = NSRect(x: 12, y: sepY, width: width - 24, height: 1)

        // + New Tab row below separator
        let newTabY = sepY + 8
        let newTabH: CGFloat = 32
        newTabRow.frame = NSRect(x: 12, y: newTabY, width: width - 24, height: newTabH)

        // Scrollable tab list below + New Tab
        let listTop = newTabY + newTabH + 6
        scrollView.frame = NSRect(x: 0, y: listTop, width: width, height: max(0, bounds.height - listTop - 8))
        stackView.frame = NSRect(x: 0, y: 0, width: width,
                                 height: max(scrollView.bounds.height, stackView.fittingSize.height))
    }

    override func mouseDown(with event: NSEvent) {
        let loc = convert(event.locationInWindow, from: nil)
        if loc.y < 46 {
            if event.clickCount == 2 {
                window?.zoom(nil)
            } else {
                window?.performDrag(with: event)
            }
            return
        }
        super.mouseDown(with: event)
    }
}

/// The "+ New Tab" row in the sidebar — uses a single attributed string so
/// the SF Symbol icon and the text share the same baseline automatically.
final class NewTabRow: NSView {
    var action: () -> Void = {}
    private let label = NSTextField(labelWithString: "")
    private var isHovered = false
    private var isPressed = false

    private let rowFont = NSFont.systemFont(ofSize: 13, weight: .medium)

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.masksToBounds = true

        label.font = rowFont
        label.textColor = Theme.chromeMutedText
        label.lineBreakMode = .byClipping
        addSubview(label)
        applyAttributedTitle(color: Theme.chromeMutedText)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    // Build the attributed string with the SF Symbol inline-attached.
    private func applyAttributedTitle(color: NSColor) {
        let font = rowFont
        let cfg  = NSImage.SymbolConfiguration(pointSize: 11, weight: .semibold)
        guard let img = NSImage(systemSymbolName: "plus", accessibilityDescription: nil)?
                .withSymbolConfiguration(cfg) else { return }

        // Tint the image by drawing into a coloured copy.
        let tinted = NSImage(size: img.size, flipped: false) { rect in
            color.setFill()
            NSBezierPath(rect: rect).fill()
            img.draw(in: rect, from: .zero, operation: .destinationIn, fraction: 1)
            return true
        }
        tinted.isTemplate = false

        let attachment = NSTextAttachment()
        attachment.image = tinted
        // Baseline-align: shift so the icon's optical centre sits on the cap-height midpoint.
        let dy = (font.capHeight - img.size.height) / 2
        attachment.bounds = CGRect(x: 0, y: dy, width: img.size.width, height: img.size.height)

        let iconStr = NSAttributedString(attachment: attachment)
        let space   = NSAttributedString(string: "  ", attributes: [.font: font, .foregroundColor: color])
        let text    = NSAttributedString(string: "New Tab", attributes: [.font: font, .foregroundColor: color])

        let combined = NSMutableAttributedString()
        combined.append(iconStr)
        combined.append(space)
        combined.append(text)
        label.attributedStringValue = combined
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        let labelH: CGFloat = 18
        label.frame = NSRect(x: 10, y: (h - labelH) / 2, width: bounds.width - 20, height: labelH)
    }

    override func draw(_ dirtyRect: NSRect) {
        if isPressed {
            Theme.pressFill.setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 8, yRadius: 8).fill()
        } else if isHovered {
            Theme.hoverFill.setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 8, yRadius: 8).fill()
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        applyAttributedTitle(color: Theme.chromeText)
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        applyAttributedTitle(color: Theme.chromeMutedText)
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        isPressed = true
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        let wasInside = isPressed
        isPressed = false
        needsDisplay = true
        if wasInside && bounds.contains(convert(event.locationInWindow, from: nil)) {
            action()
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

/// One tab in the sidebar tab list: solid white card when active (zero transparency),
/// icon/monogram + title, hover close button.
final class SidebarCellView: NSView {
    let index: Int
    var onSelect: (() -> Void)?
    var onClose: (() -> Void)?

    private let monogram = MonogramView(radius: 8)
    private let titleLabel = NSTextField(labelWithString: "")
    private let closeButton = GlyphButton(symbolName: "xmark", glyphSize: 9) {}
    private var isActive = false
    private var isHovered = false
    private var showCloseButtonOnInactiveTab = false
    private var pageColor: NSColor?

    init(index: Int) {
        self.index = index
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 10
        layer?.masksToBounds = false

        titleLabel.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        titleLabel.lineBreakMode = .byTruncatingTail

        addSubview(monogram)
        addSubview(titleLabel)

        closeButton.action = { [weak self] in self?.onClose?() }
        closeButton.isHidden = true
        addSubview(closeButton)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    func configure(title: String, url: String, active: Bool,
                   showCloseButtonOnInactiveTab: Bool, pageColor: NSColor?) {
        titleLabel.stringValue = title
        monogram.configure(title: url.isEmpty ? title : url)
        isActive = active
        self.showCloseButtonOnInactiveTab = showCloseButtonOnInactiveTab
        self.pageColor = pageColor
        updateColors()
    }

    private func updateColors() {
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        CATransaction.begin()
        CATransaction.setAnimationDuration(Theme.motionEnabled ? Theme.animationDuration : 0)
        if isActive {
            // SOLID WHITE - NO TRANSPARENCY
            let solidWhite = isDark ? NSColor(white: 0.22, alpha: 1.0) : NSColor.white
            let background = pageColor?.blended(withFraction: 0.10, of: solidWhite) ?? solidWhite
            layer?.backgroundColor = background.cgColor
            layer?.borderWidth = 0.5
            layer?.borderColor = isDark
                ? NSColor(white: 1.0, alpha: 0.12).cgColor
                : NSColor(white: 0.0, alpha: 0.04).cgColor
            layer?.shadowColor = NSColor.black.cgColor
            layer?.shadowOpacity = isDark ? 0.22 : 0.08
            layer?.shadowOffset = CGSize(width: 0, height: 2)
            layer?.shadowRadius = 5
        } else if isHovered {
            layer?.backgroundColor = Theme.cgColor(of: Theme.hoverFill, for: effectiveAppearance)
            layer?.borderWidth = 0
            layer?.shadowOpacity = 0
        } else {
            layer?.backgroundColor = NSColor.clear.cgColor
            layer?.borderWidth = 0
            layer?.shadowOpacity = 0
        }
        CATransaction.commit()
        titleLabel.font = NSFont.systemFont(ofSize: 13, weight: isActive ? .semibold : .regular)
        titleLabel.textColor = isActive ? Theme.chromeText : Theme.chromeMutedText
        closeButton.isHidden = !(isHovered || isActive || showCloseButtonOnInactiveTab)
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateColors()
    }

    override func layout() {
        super.layout()
        let height = bounds.height
        let iconSide: CGFloat = 20
        monogram.frame = NSRect(x: 10, y: (height - iconSide) / 2, width: iconSide, height: iconSide)

        let textX = monogram.frame.maxX + 10
        let closeW: CGFloat = 22
        let textW = max(20, bounds.width - textX - closeW - 6)
        let titleH: CGFloat = 18
        titleLabel.frame = NSRect(x: textX, y: (height - titleH) / 2, width: textW, height: titleH)
        closeButton.frame = NSRect(x: bounds.width - closeW - 4, y: (height - closeW) / 2, width: closeW, height: closeW)

        if bounds.width > 0 && bounds.height > 0 && isActive {
            layer?.shadowPath = CGPath(roundedRect: bounds,
                                       cornerWidth: 10,
                                       cornerHeight: 10,
                                       transform: nil)
        }
    }

    override var intrinsicContentSize: NSSize { NSSize(width: NSView.noIntrinsicMetric, height: 38) }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        Theme.animate { updateColors() }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        Theme.animate { updateColors() }
    }

    override func mouseDown(with event: NSEvent) {
        onSelect?()
    }
}

// MARK: - Floating content card

/// The rounded, bordered container the page canvas lives in — the
/// "window within the window" framing the web content.
final class ContentCardView: NSView {
    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = Theme.cardCornerRadius
        layer?.masksToBounds = false
        layer?.borderWidth = 1
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(of: Theme.cardBackground,
                                              for: effectiveAppearance)
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        layer?.borderColor = isDark
            ? NSColor(white: 1.0, alpha: 0.12).cgColor
            : NSColor(white: 1.0, alpha: 0.85).cgColor

        layer?.shadowColor = NSColor.black.cgColor
        layer?.shadowOpacity = isDark ? 0.40 : 0.13
        layer?.shadowOffset = CGSize(width: 0, height: -4)
        layer?.shadowRadius = 20
        if bounds.width > 0 && bounds.height > 0 {
            layer?.shadowPath = CGPath(roundedRect: bounds,
                                       cornerWidth: Theme.cardCornerRadius,
                                       cornerHeight: Theme.cardCornerRadius,
                                       transform: nil)
        }
        CATransaction.commit()
    }

    override func layout() {
        super.layout()
        refresh()
    }
}

// MARK: - Root chrome container

/// Owns the two layout arrangements, the backdrop and the floating card,
/// and swaps between them.
final class ChromeRootView: NSView {

    enum LayoutMode: String {
        case topBar
        case sidebar
    }

    let backdrop = CloudBackdropView(frame: .zero)
    let toolbar: ToolbarView
    let tabStrip: TabStripView
    let sidebar: SidebarView
    let loadingBar = LoadingBarView()
    let contentCard = ContentCardView()
    /// The canvas is inserted into this container inside the card so the
    /// card can clip it to rounded corners.
    private let canvasSlot = NSView()
    let statusLabel = NSTextField(labelWithString: "")
    /// Media transport strip (QTSQ audio) docked below the card; nil = no
    /// media on the active tab's page.
    private(set) var mediaBar: NSView?
    var isMediaBarVisible: Bool { mediaBar != nil }

    private(set) var mode: LayoutMode = .topBar
    private var canvasHolder: NSView?  // the active tab's canvas

    let menuDrawer = MenuDrawerView(frame: .zero)
    private(set) var isMenuOpen = false
    let drawerWidth: CGFloat = 340
    var onContentCardResized: (() -> Void)?

    init(toolbar: ToolbarView, tabStrip: TabStripView, sidebar: SidebarView) {
        self.toolbar = toolbar
        self.tabStrip = tabStrip
        self.sidebar = sidebar
        super.init(frame: .zero)

        wantsLayer = true
        addSubview(backdrop)
        addSubview(toolbar)
        addSubview(loadingBar)
        addSubview(contentCard)

        canvasSlot.wantsLayer = true
        canvasSlot.layer?.cornerRadius = Theme.cardCornerRadius
        canvasSlot.layer?.masksToBounds = true
        contentCard.addSubview(canvasSlot)

        addSubview(statusLabel)
        statusLabel.font = Theme.fontSectionLabel

        menuDrawer.isHidden = true
        menuDrawer.onClose = { [weak self] in self?.closeMenuDrawer() }
        addSubview(menuDrawer)

        apply(mode: .topBar, animated: false)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    // MARK: - Sidebar visibility
    var onSidebarVisibilityChanged: ((Bool) -> Void)?

    var isSidebarVisible = true {
        didSet {
            toolbar.showsSidebarButton = (!isSidebarVisible && mode == .sidebar)
            onSidebarVisibilityChanged?(isSidebarVisible)
            needsLayout = true
        }
    }

    func toggleSidebar() {
        if Theme.motionEnabled {
            NSAnimationContext.runAnimationGroup({ ctx in
                ctx.duration = Theme.animationDuration
                ctx.timingFunction = Theme.springTiming
                ctx.allowsImplicitAnimation = true
                self.isSidebarVisible.toggle()
                self.needsLayout = true
                self.layoutSubtreeIfNeeded()
            }, completionHandler: { [weak self] in
                self?.onContentCardResized?()
            })
        } else {
            isSidebarVisible.toggle()
            needsLayout = true
            layoutSubtreeIfNeeded()
            onContentCardResized?()
        }
    }

    // MARK: Mode switching
    func apply(mode newMode: LayoutMode, animated: Bool) {
        mode = newMode

        tabStrip.removeFromSuperview()
        sidebar.removeFromSuperview()

        switch newMode {
        case .topBar:
            addSubview(tabStrip, positioned: .below, relativeTo: toolbar)
            toolbar.showsNavButtons = true
            toolbar.showsSidebarButton = false
            toolbar.leadingInset = Theme.paddingMedium
        case .sidebar:
            addSubview(sidebar, positioned: .below, relativeTo: toolbar)
            toolbar.showsNavButtons = false
            toolbar.showsSidebarButton = !isSidebarVisible
            toolbar.leadingInset = isSidebarVisible ? 0 : 10
        }
        toolbar.buttons.forEach { $0.needsDisplay = true }

        addSubview(menuDrawer, positioned: .above, relativeTo: nil)
        needsLayout = true
    }

    // MARK: - Menu drawer presentation

    func toggleMenuDrawer() {
        if isMenuOpen {
            closeMenuDrawer()
        } else {
            openMenuDrawer()
        }
    }

    func openMenuDrawer() {
        guard !isMenuOpen else { return }
        isMenuOpen = true
        toolbar.menuButton.setOpen(true, animated: true)
        menuDrawer.reloadHistory()
        menuDrawer.isHidden = false

        if Theme.motionEnabled {
            NSAnimationContext.runAnimationGroup({ ctx in
                ctx.duration = Theme.animationDurationEntrance
                ctx.timingFunction = Theme.springTiming
                ctx.allowsImplicitAnimation = true
                self.needsLayout = true
                self.layoutSubtreeIfNeeded()
            }, completionHandler: { [weak self] in
                self?.onContentCardResized?()
            })
        } else {
            needsLayout = true
            layoutSubtreeIfNeeded()
            onContentCardResized?()
        }
    }

    func closeMenuDrawer() {
        guard isMenuOpen else { return }
        isMenuOpen = false
        toolbar.menuButton.setOpen(false, animated: true)

        if Theme.motionEnabled {
            NSAnimationContext.runAnimationGroup({ ctx in
                ctx.duration = Theme.animationDuration
                ctx.timingFunction = Theme.animationTiming
                ctx.allowsImplicitAnimation = true
                self.needsLayout = true
                self.layoutSubtreeIfNeeded()
            }, completionHandler: { [weak self] in
                guard let self, !self.isMenuOpen else { return }
                self.menuDrawer.isHidden = true
                self.onContentCardResized?()
            })
        } else {
            needsLayout = true
            layoutSubtreeIfNeeded()
            menuDrawer.isHidden = true
            onContentCardResized?()
        }
    }

    // MARK: Canvas swap

    func showCanvas(_ canvas: NSView) {
        if canvasHolder === canvas { return }
        let old = canvasHolder
        canvasHolder = canvas
        canvas.frame = canvasSlot.bounds
        canvasSlot.addSubview(canvas)
        // Micro-animation: crossfade the incoming page over the old one.
        canvas.alphaValue = Theme.motionEnabled ? 0.0 : 1.0
        Theme.animate {
            canvas.alphaValue = 1.0
            old?.removeFromSuperview()
        }
        if !Theme.motionEnabled { old?.removeFromSuperview() }
    }

    // MARK: Media bar

    /// Dock (or clear) the media transport strip below the content card.
    /// Idempotent: passing the same view (or nil over nil) is a no-op.
    func showMediaBar(_ view: NSView?) {
        if mediaBar === view { return }
        mediaBar?.removeFromSuperview()
        mediaBar = view
        if let view { addSubview(view) }
        needsLayout = true
    }

    // MARK: Layout

    private var isTopBar: Bool { mode == .topBar }

    private func layoutCard(cardTop: CGFloat, leftInset: CGFloat, rightInset: CGFloat = 0) {
        let cardX = leftInset + Theme.cardInset
        let cardY = cardTop
        let cardW = bounds.width - cardX - Theme.cardInset - rightInset
        // When the media bar is docked, the card gives up a strip above the
        // bottom inset and the bar fills it.
        let barGap: CGFloat = 8
        let barH: CGFloat = mediaBar != nil ? 46 : 0
        let reserved: CGFloat = mediaBar != nil ? barH + barGap : 0
        let cardH = bounds.height - cardY - Theme.cardInset - reserved
        contentCard.frame = NSRect(x: cardX, y: cardY, width: max(0, cardW), height: max(0, cardH))
        canvasSlot.frame = contentCard.bounds
        if let canvas = canvasHolder {
            let target = canvasSlot.bounds
            if canvas.frame != target {
                canvas.frame = target
            }
        }
        if let bar = mediaBar {
            bar.frame = NSRect(x: cardX, y: cardY + max(0, cardH) + barGap,
                               width: max(0, cardW), height: barH)
        }
    }

    override func layout() {
        super.layout()
        let w = bounds.width
        let h = bounds.height
        backdrop.frame = bounds

        let rightInset: CGFloat = isMenuOpen ? drawerWidth : 0
        let availableW = w - rightInset

        if isTopBar {
            let stripHeight = Theme.tabBarHeight
            tabStrip.frame = NSRect(x: 0, y: 0, width: availableW, height: stripHeight)
            let toolbarY = stripHeight
            toolbar.frame = NSRect(x: 0, y: toolbarY, width: availableW, height: Theme.topBarHeight)
            toolbar.layoutSubtreeIfNeeded()
            let pillBottom = toolbarY + (toolbar.addressPill.frame.height > 0
                ? toolbar.addressPill.frame.maxY
                : (Theme.topBarHeight + Theme.addressFieldHeight) / 2)
            // Equal top and bottom spacing:
            // Top spacing (pill bottom to card top) == Bottom spacing (card bottom to window bottom) == Theme.cardInset
            let cardY = pillBottom + Theme.cardInset
            loadingBar.frame = NSRect(x: 0, y: cardY - 2,
                                      width: availableW, height: 2)
            layoutCard(cardTop: cardY, leftInset: 0, rightInset: rightInset)
        } else {
            let sidebarW: CGFloat = isSidebarVisible ? Theme.sidebarWidth : 0
            sidebar.frame = NSRect(x: isSidebarVisible ? 0 : -Theme.sidebarWidth, y: 0,
                                   width: Theme.sidebarWidth, height: h)
            sidebar.isHidden = !isSidebarVisible

            let toolbarX = sidebarW
            let toolbarW = max(0, availableW - toolbarX)
            toolbar.leadingInset = isSidebarVisible ? 0 : 10
            toolbar.showsSidebarButton = !isSidebarVisible
            toolbar.frame = NSRect(x: toolbarX, y: 0,
                                   width: toolbarW, height: Theme.topBarHeight)
            toolbar.layoutSubtreeIfNeeded()
            let pillBottom = (toolbar.addressPill.frame.height > 0
                ? toolbar.addressPill.frame.maxY
                : (Theme.topBarHeight + Theme.addressFieldHeight) / 2)
            // Equal top and bottom spacing:
            // Top spacing (pill bottom to card top) == Bottom spacing (card bottom to window bottom) == Theme.cardInset
            let cardY = pillBottom + Theme.cardInset
            loadingBar.frame = NSRect(x: toolbarX, y: cardY - 2,
                                      width: toolbarW, height: 2)
            layoutCard(cardTop: cardY, leftInset: toolbarX, rightInset: rightInset)
        }

        menuDrawer.frame = NSRect(x: w - rightInset, y: 0, width: drawerWidth, height: h)

        statusLabel.isHidden = statusLabel.stringValue.isEmpty
        if !statusLabel.isHidden {
            let size = statusLabel.intrinsicContentSize
            let chipX = max(contentCard.frame.minX + 10,
                            contentCard.frame.maxX - size.width - 24)
            statusLabel.frame = NSRect(x: chipX,
                                       y: contentCard.frame.minY + contentCard.frame.height - size.height - 8,
                                       width: size.width, height: size.height)
        }
    }

    override func mouseDown(with event: NSEvent) {
        let loc = convert(event.locationInWindow, from: nil)
        if loc.y < contentCard.frame.minY {
            if event.clickCount == 2 {
                window?.zoom(nil)
            } else {
                window?.performDrag(with: event)
            }
            return
        }
        super.mouseDown(with: event)
    }
}
