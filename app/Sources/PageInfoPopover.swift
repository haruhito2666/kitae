import AppKit

// MARK: - Flipped Container Base

class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

// MARK: - Page Info Popover (Chrome / Brave-style Site Controls)

final class PageInfoPopover: NSObject, NSPopoverDelegate {
    static let shared = PageInfoPopover()

    private let popover = NSPopover()
    private let container = PageInfoContainerView()
    private var resignObserver: Any?

    override init() {
        super.init()
        popover.behavior = .transient
        popover.animates = true
        popover.delegate = self

        let vc = NSViewController()
        vc.view = container
        popover.contentViewController = vc

        container.onClose = { [weak self] in
            self?.dismiss()
        }

        container.onResize = { [weak self] newSize in
            guard let self = self else { return }
            self.popover.contentSize = newSize
            self.container.window?.hasShadow = false
        }

        // Dismiss if app is deactivated
        resignObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.dismiss()
        }
    }

    deinit {
        if let obs = resignObserver {
            NotificationCenter.default.removeObserver(obs)
        }
    }

    var isShown: Bool { popover.isShown }

    func toggle(relativeTo positioningView: NSView,
                url: String,
                usesHTTPS: Bool,
                onOpenSettings: @escaping () -> Void) {
        if popover.isShown {
            dismiss()
        } else {
            present(relativeTo: positioningView, url: url, usesHTTPS: usesHTTPS, onOpenSettings: onOpenSettings)
        }
    }

    func present(relativeTo positioningView: NSView,
                 url: String,
                 usesHTTPS: Bool,
                 onOpenSettings: @escaping () -> Void) {
        container.configure(url: url, usesHTTPS: usesHTTPS, onOpenSettings: onOpenSettings)
        popover.contentSize = container.fittingSize
        popover.show(relativeTo: positioningView.bounds, of: positioningView, preferredEdge: .maxY)

        // Remove shadow from the popover window
        container.window?.hasShadow = false
    }

    func dismiss() {
        if popover.isShown {
            popover.performClose(nil)
        }
        container.resetToMain()
    }

    func popoverDidClose(_ notification: Notification) {
        container.resetToMain()
    }
}

// MARK: - Container View

final class PageInfoContainerView: FlippedView {
    var onClose: (() -> Void)?
    var onResize: ((NSSize) -> Void)?

    private enum Page {
        case main
        case security
        case cookies
    }

    private var currentPage: Page = .main
    private var currentURL: String = ""
    private var usesHTTPS: Bool = true
    private var hstsEnforced = false
    private var hstsExpiresIn: Int64 = 0
    private var hstsIncludesSubdomains = false
    private var onOpenSettings: (() -> Void)?

    // Subviews
    private let mainView = FlippedView()
    private let securityView = FlippedView()
    private let cookiesView = FlippedView()

    override init(frame: NSRect) {
        super.init(frame: NSRect(x: 0, y: 0, width: 312, height: 184))
        wantsLayer = true

        addSubview(mainView)
        addSubview(securityView)
        addSubview(cookiesView)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        window?.hasShadow = false
    }

    func configure(url: String, usesHTTPS: Bool, onOpenSettings: @escaping () -> Void) {
        self.currentURL = url
        self.usesHTTPS = usesHTTPS
        hstsEnforced = kitae_hsts_covers(url) != 0
        hstsExpiresIn = kitae_hsts_expires_in(url)
        hstsIncludesSubdomains = kitae_hsts_includes_subdomains(url) != 0
        self.onOpenSettings = onOpenSettings
        buildMainView()
        buildSecurityView()
        buildCookiesView()
        showPage(.main)
    }

    func resetToMain() {
        showPage(.main)
    }

    private func showPage(_ page: Page) {
        currentPage = page
        mainView.isHidden = (page != .main)
        securityView.isHidden = (page != .security)
        cookiesView.isHidden = (page != .cookies)

        let targetSize = fittingSize
        frame.size = targetSize
        mainView.frame = bounds
        securityView.frame = bounds
        cookiesView.frame = bounds
        onResize?(targetSize)
    }

    override var fittingSize: NSSize {
        switch currentPage {
        case .main: return NSSize(width: 312, height: 184)
        case .security: return NSSize(width: 312, height: 252)
        case .cookies: return NSSize(width: 312, height: 118)
        }
    }

    private func displayHost(for urlString: String) -> String {
        if urlString.isEmpty || urlString == "about:start" || urlString == "about:blank" {
            return "Kitae"
        }
        if let url = URL(string: urlString), let host = url.host, !host.isEmpty {
            return host
        }
        return urlString
    }

    // MARK: - Build Main Page

    private func buildMainView() {
        mainView.subviews.forEach { $0.removeFromSuperview() }
        let host = displayHost(for: currentURL)

        // Header: Domain + Close Button
        let headerLabel = NSTextField(labelWithString: host)
        headerLabel.font = NSFont.systemFont(ofSize: 15, weight: .bold)
        headerLabel.textColor = Theme.chromeText
        headerLabel.frame = NSRect(x: 14, y: 14, width: 240, height: 22)
        mainView.addSubview(headerLabel)

        let closeBtn = GlyphButton(symbolName: "xmark", glyphSize: 12, pill: false) { [weak self] in
            self?.onClose?()
        }
        closeBtn.frame = NSRect(x: 274, y: 13, width: 24, height: 24)
        mainView.addSubview(closeBtn)

        // Row 1: Connection security
        let lockImage = usesHTTPS
            ? NSImage.tablerLockIcon
            : (NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: nil) ?? NSImage.tablerLockIcon)
        let secRow = PageInfoRowButton(
            image: lockImage,
            iconColor: usesHTTPS ? Theme.chromeText : Theme.accent,
            title: usesHTTPS ? "Connection uses HTTPS" : "Connection is not encrypted",
            trailingSymbol: "chevron.right"
        ) { [weak self] in
            self?.showPage(.security)
        }
        secRow.frame = NSRect(x: 12, y: 46, width: 288, height: 40)
        mainView.addSubview(secRow)

        // Row 2: Cookies and site data
        let cookieRow = PageInfoRowButton(
            image: .tablerCookieIcon,
            iconColor: Theme.chromeText,
            title: "Cookies and site data",
            trailingSymbol: "chevron.right"
        ) { [weak self] in
            self?.showPage(.cookies)
        }
        cookieRow.frame = NSRect(x: 12, y: 90, width: 288, height: 40)
        mainView.addSubview(cookieRow)

        // Row 3: Site settings
        let settingsRow = PageInfoRowButton(
            image: .tablerSettingsIcon,
            iconColor: Theme.chromeText,
            title: "Site settings",
            trailingSymbol: "arrow.up.forward.app"
        ) { [weak self] in
            self?.onClose?()
            self?.onOpenSettings?()
        }
        settingsRow.frame = NSRect(x: 12, y: 134, width: 288, height: 40)
        mainView.addSubview(settingsRow)
    }

    // MARK: - Build Security Subpage

    private func buildSecurityView() {
        securityView.subviews.forEach { $0.removeFromSuperview() }
        let host = displayHost(for: currentURL)

        // Back button
        let backBtn = GlyphButton(symbolName: "arrow.left", glyphSize: 12, pill: false) { [weak self] in
            self?.showPage(.main)
        }
        backBtn.frame = NSRect(x: 10, y: 13, width: 24, height: 24)
        securityView.addSubview(backBtn)

        // Title + subtitle
        let titleLabel = NSTextField(labelWithString: "Security")
        titleLabel.font = NSFont.systemFont(ofSize: 14, weight: .bold)
        titleLabel.textColor = Theme.chromeText
        titleLabel.frame = NSRect(x: 40, y: 9, width: 220, height: 18)
        securityView.addSubview(titleLabel)

        let subLabel = NSTextField(labelWithString: host)
        subLabel.font = NSFont.systemFont(ofSize: 11, weight: .regular)
        subLabel.textColor = Theme.chromeMutedText
        subLabel.frame = NSRect(x: 40, y: 27, width: 220, height: 15)
        securityView.addSubview(subLabel)

        let closeBtn = GlyphButton(symbolName: "xmark", glyphSize: 12, pill: false) { [weak self] in
            self?.onClose?()
        }
        closeBtn.frame = NSRect(x: 274, y: 13, width: 24, height: 24)
        securityView.addSubview(closeBtn)

        let divider = HairlineView(frame: NSRect(x: 0, y: 48, width: 312, height: 1))
        securityView.addSubview(divider)

        // Security description card
        let lockImageView = NSImageView(frame: NSRect(x: 14, y: 60, width: 16, height: 16))
        let lockImg = usesHTTPS
            ? NSImage.tablerLockIcon
            : (NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: nil) ?? NSImage.tablerLockIcon)
        lockImageView.image = lockImg
        lockImageView.imageScaling = .scaleProportionallyUpOrDown
        lockImageView.contentTintColor = usesHTTPS ? Theme.chromeText : Theme.accent
        securityView.addSubview(lockImageView)

        let secTitle = NSTextField(labelWithString: hstsEnforced
            ? "HTTPS enforced by site policy"
            : (usesHTTPS ? "Connection uses HTTPS" : "Connection is not encrypted"))
        secTitle.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        secTitle.textColor = Theme.chromeText
        secTitle.frame = NSRect(x: 38, y: 58, width: 260, height: 18)
        securityView.addSubview(secTitle)

        let descText: String
        if hstsEnforced {
            let scope = hstsIncludesSubdomains ? "including subdomains" : "for this host"
            descText = "HTTP navigations are upgraded automatically by HSTS \(scope). The policy expires in \(max(1, Int(hstsExpiresIn))) seconds."
        } else if usesHTTPS {
            descText = "This page uses HTTPS for transport encryption. Certificate details are not exposed in this view."
        } else {
            descText = "This page uses unencrypted HTTP. You should not enter sensitive information on this site."
        }
        let secDesc = NSTextField(wrappingLabelWithString: descText)
        secDesc.font = NSFont.systemFont(ofSize: 12, weight: .regular)
        secDesc.textColor = Theme.chromeMutedText
        secDesc.frame = NSRect(x: 38, y: 80, width: 260, height: 56)
        securityView.addSubview(secDesc)

        let divider2 = HairlineView(frame: NSRect(x: 0, y: 146, width: 312, height: 1))
        securityView.addSubview(divider2)

        // Certificate row
        let certRow = PageInfoRowButton(
            image: NSImage(systemSymbolName: "questionmark.circle", accessibilityDescription: nil) ?? NSImage.tablerLockIcon,
            iconColor: Theme.chromeText,
            title: "Certificate details unavailable",
            trailingSymbol: "chevron.right"
        ) {
            let alert = NSAlert()
            alert.messageText = "Certificate Details"
            alert.informativeText = self.usesHTTPS
                ? "This page uses HTTPS, but Kitae does not expose the certificate issuer or validity dates in this view."
                : "This page uses unencrypted HTTP and has no HTTPS certificate."
            alert.alertStyle = .informational
            alert.addButton(withTitle: "OK")
            alert.runModal()
        }
        certRow.frame = NSRect(x: 12, y: 154, width: 288, height: 40)
        securityView.addSubview(certRow)

        let hstsRow = PageInfoRowButton(
            image: NSImage(systemSymbolName: hstsEnforced ? "lock.shield" : "lock.open",
                           accessibilityDescription: nil) ?? NSImage.tablerLockIcon,
            iconColor: hstsEnforced ? Theme.accent : Theme.chromeMutedText,
            title: hstsEnforced ? "HSTS is active for this site" : "HSTS is not active",
            subtitle: hstsEnforced ? "Select to delete the stored policy" : nil,
            trailingSymbol: hstsEnforced ? "trash" : nil
        ) { [weak self] in
            guard let self, self.hstsEnforced else { return }
            let alert = NSAlert()
            alert.messageText = "Delete HSTS Policy?"
            alert.informativeText = "Future HTTP navigations to this host will no longer be upgraded automatically."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Cancel")
            alert.addButton(withTitle: "Delete")
            if alert.runModal() == .alertSecondButtonReturn {
                guard kitae_clear_hsts(self.currentURL) != 0 else { return }
                HstsStore.shared.saveNow()
                self.hstsEnforced = false
                self.hstsExpiresIn = 0
                self.hstsIncludesSubdomains = false
                self.buildSecurityView()
                self.showPage(.security)
            }
        }
        hstsRow.frame = NSRect(x: 12, y: 200, width: 288, height: 44)
        securityView.addSubview(hstsRow)
    }

    // MARK: - Build Cookies Subpage

    private func buildCookiesView() {
        cookiesView.subviews.forEach { $0.removeFromSuperview() }
        let host = displayHost(for: currentURL)

        // Back button
        let backBtn = GlyphButton(symbolName: "arrow.left", glyphSize: 12, pill: false) { [weak self] in
            self?.showPage(.main)
        }
        backBtn.frame = NSRect(x: 10, y: 13, width: 24, height: 24)
        cookiesView.addSubview(backBtn)

        // Title + subtitle
        let titleLabel = NSTextField(labelWithString: "Cookies and site data")
        titleLabel.font = NSFont.systemFont(ofSize: 14, weight: .bold)
        titleLabel.textColor = Theme.chromeText
        titleLabel.frame = NSRect(x: 40, y: 9, width: 220, height: 18)
        cookiesView.addSubview(titleLabel)

        let subLabel = NSTextField(labelWithString: host)
        subLabel.font = NSFont.systemFont(ofSize: 11, weight: .regular)
        subLabel.textColor = Theme.chromeMutedText
        subLabel.frame = NSRect(x: 40, y: 27, width: 220, height: 15)
        cookiesView.addSubview(subLabel)

        let closeBtn = GlyphButton(symbolName: "xmark", glyphSize: 12, pill: false) { [weak self] in
            self?.onClose?()
        }
        closeBtn.frame = NSRect(x: 274, y: 13, width: 24, height: 24)
        cookiesView.addSubview(closeBtn)

        let divider = HairlineView(frame: NSRect(x: 0, y: 48, width: 312, height: 1))
        cookiesView.addSubview(divider)

        // Storage details row
        let storageRow = PageInfoRowButton(
            image: .tablerCookieIcon,
            iconColor: Theme.chromeText,
            title: "Manage on-device site data",
            subtitle: "Cookies are kept in memory for this app session",
            trailingSymbol: "arrow.up.forward.app"
        ) {
            let alert = NSAlert()
            alert.messageText = "On-Device Site Data"
            alert.informativeText = "Kitae stores cookies in memory for the current app session. They are discarded when the app quits."
            alert.alertStyle = .informational
            alert.addButton(withTitle: "OK")
            alert.runModal()
        }
        storageRow.frame = NSRect(x: 12, y: 58, width: 288, height: 48)
        cookiesView.addSubview(storageRow)
    }
}

// MARK: - Row Button Component

final class PageInfoRowButton: FlippedView {
    private let iconImageView = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let subtitleLabel = NSTextField(labelWithString: "")
    private let trailingImageView = NSImageView()
    private var trackingArea: NSTrackingArea?

    private var isHovered = false { didSet { needsDisplay = true } }
    private var isPressed = false { didSet { needsDisplay = true } }
    var action: () -> Void

    init(image: NSImage,
         iconColor: NSColor,
         title: String,
         subtitle: String? = nil,
         trailingSymbol: String? = nil,
         action: @escaping () -> Void) {
        self.action = action
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(subtitle.map { "\(title), \($0)" } ?? title)
        wantsLayer = true
        layer?.cornerRadius = 8

        // Icon — exact 16×16 pt size (divisible by 2)
        iconImageView.image = image
        iconImageView.imageScaling = .scaleProportionallyUpOrDown
        iconImageView.contentTintColor = iconColor
        addSubview(iconImageView)

        // Title
        titleLabel.stringValue = title
        titleLabel.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        titleLabel.textColor = Theme.chromeText
        addSubview(titleLabel)

        // Subtitle (optional)
        if let subtitle {
            subtitleLabel.stringValue = subtitle
            subtitleLabel.font = NSFont.systemFont(ofSize: 11, weight: .regular)
            subtitleLabel.textColor = Theme.chromeMutedText
            addSubview(subtitleLabel)
        }

        // Trailing symbol
        if let trailingSymbol {
            trailingImageView.image = NSImage(systemSymbolName: trailingSymbol, accessibilityDescription: nil)
            trailingImageView.imageScaling = .scaleProportionallyUpOrDown
            trailingImageView.contentTintColor = Theme.chromeMutedText
            addSubview(trailingImageView)
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var acceptsFirstResponder: Bool { true }

    override func accessibilityPerformPress() -> Bool {
        action()
        return true
    }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 76 || event.keyCode == 49 {
            _ = accessibilityPerformPress()
        } else {
            super.keyDown(with: event)
        }
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        let w = bounds.width

        // 16×16 icon, 10pt from left, vertically centered
        iconImageView.frame = NSRect(x: 10, y: (h - 16) / 2, width: 16, height: 16)

        // Text margin 36pt (10 + 16 + 10)
        if subtitleLabel.superview != nil {
            titleLabel.frame = NSRect(x: 36, y: (h - 32) / 2, width: w - 36 - 28, height: 17)
            subtitleLabel.frame = NSRect(x: 36, y: titleLabel.frame.maxY, width: w - 36 - 28, height: 15)
        } else {
            titleLabel.frame = NSRect(x: 36, y: (h - 18) / 2, width: w - 36 - 28, height: 18)
        }

        // 12×12 trailing symbol, 10pt from right edge, vertically centered
        trailingImageView.frame = NSRect(x: w - 10 - 12, y: (h - 12) / 2, width: 12, height: 12)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let ta = trackingArea { removeTrackingArea(ta) }
        let ta = NSTrackingArea(rect: bounds,
                                options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                owner: self, userInfo: nil)
        addTrackingArea(ta)
        trackingArea = ta
    }

    override func mouseEntered(with event: NSEvent) { isHovered = true }
    override func mouseExited(with event: NSEvent) { isHovered = false; isPressed = false }
    override func mouseDown(with event: NSEvent) { isPressed = true }
    override func mouseUp(with event: NSEvent) {
        let wasPressed = isPressed
        isPressed = false
        if wasPressed && bounds.contains(convert(event.locationInWindow, from: nil)) {
            action()
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        if isPressed || isHovered {
            (isPressed ? Theme.pressFill : Theme.hoverFill).setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 8, yRadius: 8).fill()
        }
    }
}
