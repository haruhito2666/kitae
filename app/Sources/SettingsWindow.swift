// SettingsWindow.swift — Dedicated Settings window for Kitae.
// Implements a clean, modern two-column structured form layout with icon sidebar,
// back/forward navigation header, miniature tab layout preview cards,
// and strictly functional preferences (no clutter or unnecessary essays).

import AppKit

// MARK: - Controller

final class SettingsWindowController: NSWindowController, NSWindowDelegate {

    enum Section: Int, CaseIterable {
        case appearance = 0
        case tabs = 1
        case privacy = 2
        case advanced = 3
        case about = 4

        var title: String {
            switch self {
            case .appearance: return "Appearance"
            case .tabs: return "Tabs"
            case .privacy: return "Privacy"
            case .advanced: return "Advanced"
            case .about: return "About"
            }
        }

        var symbol: String {
            switch self {
            case .appearance: return "paintpalette.fill"
            case .tabs: return "macwindow.on.rectangle"
            case .privacy: return "shield.lefthalf.filled"
            case .advanced: return "gearshape.2.fill"
            case .about: return "info.circle.fill"
            }
        }
    }

    private let sidebarView = SettingsSidebar()
    private let paneContainer = NSView()
    private let headerView = SettingsPaneHeader()
    private var currentPane: NSView?
    private var currentSection: Section = .appearance
    var isWindowOpen = false

    // Navigation history for back / forward
    private var historyStack: [Section] = []
    private var forwardStack: [Section] = []
    private var isNavigatingHistory = false

    var onLayoutChange: (ChromeRootView.LayoutMode) -> Void = { _ in }
    var onAppearanceChange: (AppearanceMode) -> Void = { _ in }
    var onMotionReducedChange: (Bool) -> Void = { _ in }
    var onGpuChange: (Bool) -> Void = { _ in }
    var onSmoothScrollingChange: (Bool) -> Void = { _ in }
    var onShowInactiveCloseButtonsChange: (Bool) -> Void = { _ in }
    var onExtendPageColorChange: (Bool) -> Void = { _ in }
    var onWarnBeforeClosingLastTabChange: (Bool) -> Void = { _ in }
    var onTabCleanupPolicyChange: (TabCleanupPolicy) -> Void = { _ in }
    var onClearHistoryOnQuitChange: (Bool) -> Void = { _ in }
    var onAlwaysShowFullURLChange: (Bool) -> Void = { _ in }
    var onHighlightTopLevelDomainChange: (Bool) -> Void = { _ in }
    var onShowDebugBoundsChange: (Bool) -> Void = { _ in }
    var onShowDiagnostics: (() -> Void)?
    var onHTTPSOnlyChange: (Bool) -> Void = { _ in }
    var onWarnInsecureHTTPChange: (Bool) -> Void = { _ in }
    var onBlockThirdPartyCookiesChange: (Bool) -> Void = { _ in }
    var onDNTChange: (Bool) -> Void = { _ in }
    var onSearchProviderChange: (SearchProviderSetting) -> Void = { _ in }
    var onClearHistory: (() -> Void)?
    var onClearPageCache: (() -> Void)?
    var onReset: (() -> Void)?

    private var appearancePane: AppearancePane!
    private var tabsPane: TabsPane!
    private var privacyPane: PrivacyPane!
    private var advancedPane: AdvancedPane!
    private var aboutPane: AboutPane!

    init(currentLayout: ChromeRootView.LayoutMode, currentAppearance: AppearanceMode,
         motionReduced: Bool, gpuEnabled: Bool) {
        // Spacious 820 × 530 pt window
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 820, height: 530),
            styleMask: [.titled, .closable, .miniaturizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        window.title = "Kitae Settings"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isReleasedWhenClosed = false
        window.backgroundColor = Theme.cardBackground
        window.isMovableByWindowBackground = true

        // Explicitly remove window shadow as requested
        window.hasShadow = false
        window.hidesOnDeactivate = true
        window.level = .normal
        window.collectionBehavior = [.managed, .participatesInCycle]

        super.init(window: window)
        window.delegate = self

        // Automatically hide when user switches to other applications
        NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.window?.orderOut(nil)
        }

        NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            guard let self = self else { return }
            if self.isWindowOpen {
                self.window?.orderFront(nil)
            }
        }

        appearancePane = AppearancePane(current: currentAppearance,
                                        motionReduced: motionReduced,
                                        gpuEnabled: gpuEnabled,
                                        smoothScrolling: SettingsStore.shared.smoothPageScrolling)
        tabsPane = TabsPane(current: currentLayout,
                            showInactiveCloseButtons: SettingsStore.shared.showInactiveTabCloseButtons,
                            warnBeforeClosingLastTab: SettingsStore.shared.warnBeforeClosingLastTab,
                            cleanupPolicy: SettingsStore.shared.tabCleanupPolicy,
                            extendPageColor: SettingsStore.shared.extendPageColor)
        privacyPane = PrivacyPane(
            clearHistoryOnQuit: SettingsStore.shared.clearHistoryOnQuit,
            httpsOnly: SettingsStore.shared.httpsOnly,
            warnInsecureHTTP: SettingsStore.shared.warnInsecureHTTP,
            blockThirdPartyCookies: SettingsStore.shared.blockThirdPartyCookies,
            sendDNT: SettingsStore.shared.sendDNT
        )
        advancedPane = AdvancedPane(
            alwaysShowFullURL: SettingsStore.shared.alwaysShowFullURL,
            searchProvider: SettingsStore.shared.searchProvider,
            highlightTopLevelDomain: SettingsStore.shared.highlightTopLevelDomain,
            showDebugBounds: SettingsStore.shared.showDebugBounds
        )
        aboutPane = AboutPane()

        let content = FlippedView(frame: window.contentLayoutRect)
        content.wantsLayer = true

        let sidebarW: CGFloat = 200
        sidebarView.frame = NSRect(x: 0, y: 0, width: sidebarW, height: 530)
        sidebarView.autoresizingMask = [.height]
        sidebarView.onSelect = { [weak self] section in
            self?.navigateTo(section, recordHistory: true)
        }
        content.addSubview(sidebarView)

        // Vertical divider between sidebar and content
        let divider = HairlineView(frame: NSRect(x: sidebarW - 1, y: 0, width: 1, height: 530))
        divider.autoresizingMask = [.height]
        content.addSubview(divider)

        // Content Area (Header + Panes)
        let contentX = sidebarW
        let contentW = 820 - sidebarW

        headerView.frame = NSRect(x: contentX, y: 0, width: contentW, height: 58)
        headerView.autoresizingMask = [.width]
        headerView.onBack = { [weak self] in self?.goBack() }
        headerView.onForward = { [weak self] in self?.goForward() }
        content.addSubview(headerView)

        paneContainer.frame = NSRect(x: contentX, y: 58, width: contentW, height: 530 - 58)
        paneContainer.autoresizingMask = [.width, .height]
        content.addSubview(paneContainer)

        // Wire callbacks
        appearancePane.onAppearanceChange = { [weak self] mode in
            self?.onAppearanceChange(mode)
        }
        appearancePane.onMotionReducedChange = { [weak self] reduced in
            self?.onMotionReducedChange(reduced)
        }
        appearancePane.onGpuChange = { [weak self] on in
            self?.onGpuChange(on)
        }
        appearancePane.onSmoothScrollingChange = { [weak self] on in
            self?.onSmoothScrollingChange(on)
        }
        tabsPane.onLayoutChange = { [weak self] mode in
            self?.onLayoutChange(mode)
        }
        tabsPane.onShowInactiveCloseButtonsChange = { [weak self] on in
            self?.onShowInactiveCloseButtonsChange(on)
        }
        tabsPane.onExtendPageColorChange = { [weak self] on in
            self?.onExtendPageColorChange(on)
        }
        tabsPane.onWarnBeforeClosingLastTabChange = { [weak self] on in
            self?.onWarnBeforeClosingLastTabChange(on)
        }
        tabsPane.onCleanupPolicyChange = { [weak self] policy in
            self?.onTabCleanupPolicyChange(policy)
        }
        privacyPane.onClearHistoryOnQuitChange = { [weak self] on in
            self?.onClearHistoryOnQuitChange(on)
        }
        privacyPane.onHTTPSOnlyChange = { [weak self] on in
            self?.onHTTPSOnlyChange(on)
        }
        privacyPane.onWarnInsecureHTTPChange = { [weak self] on in
            self?.onWarnInsecureHTTPChange(on)
        }
        privacyPane.onBlockThirdPartyCookiesChange = { [weak self] on in
            self?.onBlockThirdPartyCookiesChange(on)
        }
        privacyPane.onDNTChange = { [weak self] on in
            self?.onDNTChange(on)
        }
        privacyPane.onClearHistory = { [weak self] in
            self?.onClearHistory?()
        }
        privacyPane.onClearPageCache = { [weak self] in
            self?.onClearPageCache?()
        }
        advancedPane.onAlwaysShowFullURLChange = { [weak self] on in
            self?.onAlwaysShowFullURLChange(on)
        }
        advancedPane.onSearchProviderChange = { [weak self] provider in
            self?.onSearchProviderChange(provider)
        }
        advancedPane.onHighlightTopLevelDomainChange = { [weak self] on in
            self?.onHighlightTopLevelDomainChange(on)
        }
        advancedPane.onShowDebugBoundsChange = { [weak self] on in
            self?.onShowDebugBoundsChange(on)
        }
        advancedPane.onShowDiagnostics = { [weak self] in
            self?.onShowDiagnostics?()
        }
        advancedPane.onReset = { [weak self] in
            self?.onReset?()
        }

        window.contentView = content
        navigateTo(.appearance, recordHistory: false)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func navigateTo(_ section: Section, recordHistory: Bool) {
        guard section != currentSection || currentPane == nil else { return }

        if recordHistory && !isNavigatingHistory {
            historyStack.append(currentSection)
            forwardStack.removeAll()
        }

        currentSection = section
        sidebarView.setSelected(section.rawValue)
        headerView.setTitle(section.title)
        headerView.setNavigationEnabled(canGoBack: !historyStack.isEmpty,
                                       canGoForward: !forwardStack.isEmpty)

        let pane: NSView
        switch section {
        case .appearance: pane = appearancePane
        case .tabs: pane = tabsPane
        case .privacy: pane = privacyPane
        case .advanced: pane = advancedPane
        case .about: pane = aboutPane
        }
        pane.frame = paneContainer.bounds
        pane.autoresizingMask = [.width, .height]

        let old = currentPane
        currentPane = pane
        paneContainer.addSubview(pane)
        old?.removeFromSuperview()

        if Theme.motionEnabled {
            pane.alphaValue = 0
            Theme.animateEntrance { pane.alphaValue = 1 }
        } else {
            pane.alphaValue = 1
        }
    }

    private func goBack() {
        guard let prev = historyStack.popLast() else { return }
        forwardStack.append(currentSection)
        isNavigatingHistory = true
        navigateTo(prev, recordHistory: false)
        isNavigatingHistory = false
        headerView.setNavigationEnabled(canGoBack: !historyStack.isEmpty,
                                       canGoForward: !forwardStack.isEmpty)
    }

    private func goForward() {
        guard let next = forwardStack.popLast() else { return }
        historyStack.append(currentSection)
        isNavigatingHistory = true
        navigateTo(next, recordHistory: false)
        isNavigatingHistory = false
        headerView.setNavigationEnabled(canGoBack: !historyStack.isEmpty,
                                       canGoForward: !forwardStack.isEmpty)
    }

    func refreshFromSettings() {
        appearancePane.updateSettings(current: SettingsStore.shared.appearanceMode,
                                      motionReduced: SettingsStore.shared.motionReduced,
                                      gpuEnabled: SettingsStore.shared.gpuEnabled,
                                      smoothScrolling: SettingsStore.shared.smoothPageScrolling)
        tabsPane.updateSettings(showInactiveCloseButtons: SettingsStore.shared.showInactiveTabCloseButtons,
                                warnBeforeClosingLastTab: SettingsStore.shared.warnBeforeClosingLastTab,
                                cleanupPolicy: SettingsStore.shared.tabCleanupPolicy,
                                extendPageColor: SettingsStore.shared.extendPageColor)
        privacyPane.updateSettings(
            clearHistoryOnQuit: SettingsStore.shared.clearHistoryOnQuit,
            httpsOnly: SettingsStore.shared.httpsOnly,
            warnInsecureHTTP: SettingsStore.shared.warnInsecureHTTP,
            blockThirdPartyCookies: SettingsStore.shared.blockThirdPartyCookies,
            sendDNT: SettingsStore.shared.sendDNT
        )
        advancedPane.updateSettings(
            alwaysShowFullURL: SettingsStore.shared.alwaysShowFullURL,
            searchProvider: SettingsStore.shared.searchProvider,
            highlightTopLevelDomain: SettingsStore.shared.highlightTopLevelDomain,
            showDebugBounds: SettingsStore.shared.showDebugBounds
        )
    }

    func windowWillClose(_ notification: Notification) {
        isWindowOpen = false
        if let win = window, let parent = win.parent {
            parent.removeChildWindow(win)
        }
    }
}

// MARK: - Header View

final class SettingsPaneHeader: FlippedView {
    private let backButton = GlyphButton(symbolName: "chevron.left", glyphSize: 12)
    private let forwardButton = GlyphButton(symbolName: "chevron.right", glyphSize: 12)
    private let titleLabel = NSTextField(labelWithString: "")

    var onBack: (() -> Void)?
    var onForward: (() -> Void)?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true

        titleLabel.font = NSFont.systemFont(ofSize: 17, weight: .bold)
        titleLabel.textColor = Theme.chromeText

        backButton.action = { [weak self] in self?.onBack?() }
        forwardButton.action = { [weak self] in self?.onForward?() }

        addSubview(backButton)
        addSubview(forwardButton)
        addSubview(titleLabel)

        setNavigationEnabled(canGoBack: false, canGoForward: false)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func setTitle(_ title: String) {
        titleLabel.stringValue = title
    }

    func setNavigationEnabled(canGoBack: Bool, canGoForward: Bool) {
        backButton.alphaValue = canGoBack ? 1.0 : 0.35
        forwardButton.alphaValue = canGoForward ? 1.0 : 0.35
    }

    override func layout() {
        super.layout()
        let topY: CGFloat = 20
        backButton.frame = NSRect(x: 24, y: topY, width: 22, height: 22)
        forwardButton.frame = NSRect(x: 48, y: topY, width: 22, height: 22)
        titleLabel.frame = NSRect(x: 80, y: topY - 1, width: bounds.width - 96, height: 24)
    }
}

// MARK: - Sidebar

final class SettingsSidebar: FlippedView {
    var onSelect: ((SettingsWindowController.Section) -> Void)?
    private var rows: [SettingsRowView] = []
    private let header = NSTextField(labelWithString: "SETTINGS")

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = Theme.cgColor(of: Theme.sidebarBackground, for: effectiveAppearance)

        header.font = NSFont.systemFont(ofSize: 11, weight: .bold)
        header.textColor = Theme.chromeMutedText
        addSubview(header)

        for section in SettingsWindowController.Section.allCases {
            let row = SettingsRowView(title: section.title, symbolName: section.symbol)
            row.onSelect = { [weak self] in self?.onSelect?(section) }
            addSubview(row)
            rows.append(row)
        }
        rows.first?.setSelected(true)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        layer?.backgroundColor = Theme.cgColor(of: Theme.sidebarBackground, for: effectiveAppearance)
    }

    func setSelected(_ index: Int) {
        for (i, row) in rows.enumerated() { row.setSelected(i == index) }
    }

    override func layout() {
        super.layout()
        header.frame = NSRect(x: 20, y: 50, width: bounds.width - 40, height: 16)

        let startY: CGFloat = 74
        let rowHeight: CGFloat = 34
        let rowSpacing: CGFloat = 4
        for (i, row) in rows.enumerated() {
            row.frame = NSRect(x: 10, y: startY + CGFloat(i) * (rowHeight + rowSpacing),
                               width: bounds.width - 20, height: rowHeight)
        }
    }
}

/// One sidebar row: 16×16 pt icon + title.
/// Solid accent pill with white text & white icon when selected.
final class SettingsRowView: FlippedView {
    private let titleField: NSTextField
    private let iconImageView = NSImageView()
    private var selected = false
    private var isHovered = false
    var onSelect: (() -> Void)?

    init(title: String, symbolName: String) {
        titleField = NSTextField(labelWithString: title)
        titleField.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        titleField.textColor = Theme.chromeText

        iconImageView.image = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil)
        iconImageView.imageScaling = .scaleProportionallyUpOrDown
        iconImageView.contentTintColor = Theme.chromeMutedText

        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 6

        addSubview(iconImageView)
        addSubview(titleField)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func setSelected(_ value: Bool) {
        selected = value
        refresh()
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setAnimationDuration(Theme.motionEnabled ? Theme.animationDuration : 0)

        if selected {
            layer?.backgroundColor = Theme.cgColor(of: Theme.accent, for: effectiveAppearance)
            iconImageView.contentTintColor = .white
            titleField.textColor = .white
            titleField.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        } else {
            layer?.backgroundColor = Theme.cgColor(
                of: isHovered ? Theme.hoverFill : .clear, for: effectiveAppearance)
            iconImageView.contentTintColor = Theme.chromeMutedText
            titleField.textColor = Theme.chromeText
            titleField.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        }
        CATransaction.commit()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    override func layout() {
        super.layout()
        // 16×16 pt icon (divisible by 2) centered vertically
        iconImageView.frame = NSRect(x: 10, y: (bounds.height - 16) / 2, width: 16, height: 16)
        titleField.frame = NSRect(x: 34, y: (bounds.height - 18) / 2, width: bounds.width - 40, height: 18)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach { removeTrackingArea($0) }
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self, userInfo: nil))
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        if !selected { refresh() }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        if !selected { refresh() }
    }

    override func mouseDown(with event: NSEvent) {
        onSelect?()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Two-Column Form Group View

/// Structured two-column group container:
/// Right-aligned label on the left (width: 130pt), control views on the right (x: 150pt).
final class SettingsFormGroup: FlippedView {
    private let labelField = NSTextField(labelWithString: "")
    private let controls: [NSView]
    private let rowSpacing: CGFloat

    init(label: String, controls: [NSView], rowSpacing: CGFloat = 8) {
        self.controls = controls
        self.rowSpacing = rowSpacing
        super.init(frame: .zero)

        labelField.stringValue = label
        labelField.alignment = .right
        labelField.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        labelField.textColor = Theme.chromeText
        addSubview(labelField)

        for control in controls {
            addSubview(control)
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    var measuredHeight: CGFloat {
        var totalH: CGFloat = 0
        for (i, control) in controls.enumerated() {
            totalH += control.frame.height
            if i < controls.count - 1 { totalH += rowSpacing }
        }
        return max(24, totalH)
    }

    override func layout() {
        super.layout()
        let labelW: CGFloat = 130
        let controlX: CGFloat = 150
        let controlW = max(100, bounds.width - controlX - 30)

        labelField.frame = NSRect(x: 0, y: 2, width: labelW, height: 18)

        var y: CGFloat = 0
        for control in controls {
            control.frame = NSRect(x: controlX, y: y, width: controlW, height: control.frame.height)
            y += control.frame.height + rowSpacing
        }
    }
}

// MARK: - Panes Base

class SettingsPane: FlippedView {
    private var groups: [SettingsFormGroup] = []

    func addGroup(_ group: SettingsFormGroup) {
        groups.append(group)
        addSubview(group)
    }

    override func layout() {
        super.layout()
        var y: CGFloat = 16
        let groupSpacing: CGFloat = 26
        for group in groups {
            let h = group.measuredHeight
            group.frame = NSRect(x: 20, y: y, width: bounds.width - 40, height: h)
            y += h + groupSpacing
        }
    }
}

// MARK: - Tabs Pane (Layout Preview Cards & Functional Options)

final class TabsPane: SettingsPane {
    private var cards: [LayoutPreviewCard] = []
    private let warnBeforeClosingLastTabCheck = CheckRowView(title: "Warn before closing last tab in a window")
    private let showInactiveCloseButtonsCheck = CheckRowView(title: "Show close button on inactive tabs")
    private let extendPageColorCheck = CheckRowView(title: "Extend website color into tab bar")
    private let cleanupPopup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 180, height: 26), pullsDown: false)

    var onLayoutChange: (ChromeRootView.LayoutMode) -> Void = { _ in }
    var onWarnBeforeClosingLastTabChange: (Bool) -> Void = { _ in }
    var onShowInactiveCloseButtonsChange: (Bool) -> Void = { _ in }
    var onExtendPageColorChange: (Bool) -> Void = { _ in }
    var onCleanupPolicyChange: (TabCleanupPolicy) -> Void = { _ in }

    init(current: ChromeRootView.LayoutMode,
         showInactiveCloseButtons: Bool,
         warnBeforeClosingLastTab: Bool,
         cleanupPolicy: TabCleanupPolicy,
         extendPageColor: Bool) {
        super.init(frame: .zero)

        let topCard = LayoutPreviewCard(mode: .topBar)
        let sideCard = LayoutPreviewCard(mode: .sidebar)
        cards = [topCard, sideCard]

        let cardsContainer = FlippedView(frame: NSRect(x: 0, y: 0, width: 380, height: 110))
        topCard.frame = NSRect(x: 0, y: 0, width: 140, height: 110)
        sideCard.frame = NSRect(x: 156, y: 0, width: 140, height: 110)
        cardsContainer.addSubview(topCard)
        cardsContainer.addSubview(sideCard)

        for card in cards {
            card.onSelect = { [weak self] in
                guard let self else { return }
                for other in self.cards { other.setSelected(other.mode == card.mode, animated: true) }
                self.onLayoutChange(card.mode)
            }
        }
        topCard.setSelected(current == .topBar, animated: false)
        sideCard.setSelected(current == .sidebar, animated: false)

        addGroup(SettingsFormGroup(label: "Tab Layout:", controls: [cardsContainer]))

        warnBeforeClosingLastTabCheck.isChecked = warnBeforeClosingLastTab
        warnBeforeClosingLastTabCheck.frame.size.height = 22
        showInactiveCloseButtonsCheck.isChecked = showInactiveCloseButtons
        showInactiveCloseButtonsCheck.frame.size.height = 22
        extendPageColorCheck.isChecked = extendPageColor
        extendPageColorCheck.frame.size.height = 22
        warnBeforeClosingLastTabCheck.onChange = { [weak self] on in
            self?.onWarnBeforeClosingLastTabChange(on)
        }
        showInactiveCloseButtonsCheck.onChange = { [weak self] on in
            self?.onShowInactiveCloseButtonsChange(on)
        }
        extendPageColorCheck.onChange = { [weak self] on in
            self?.onExtendPageColorChange(on)
        }
        addGroup(SettingsFormGroup(label: "General:",
                                   controls: [warnBeforeClosingLastTabCheck,
                                              showInactiveCloseButtonsCheck,
                                              extendPageColorCheck],
                                   rowSpacing: 8))

        cleanupPopup.addItems(withTitles: TabCleanupPolicy.allCases.map(\.title))
        cleanupPopup.target = self
        cleanupPopup.action = #selector(cleanupChanged)
        cleanupPopup.selectItem(at: cleanupPolicy.popupIndex)

        let cleanupContainer = FlippedView(frame: NSRect(x: 0, y: 0, width: 400, height: 26))
        let cleanupLabel = NSTextField(labelWithString: "Clean up tabs I haven't used in:")
        cleanupLabel.font = NSFont.systemFont(ofSize: 13, weight: .regular)
        cleanupLabel.textColor = Theme.chromeText
        cleanupLabel.frame = NSRect(x: 0, y: 3, width: 195, height: 20)
        cleanupPopup.frame = NSRect(x: 200, y: 0, width: 150, height: 26)
        cleanupContainer.addSubview(cleanupLabel)
        cleanupContainer.addSubview(cleanupPopup)

        addGroup(SettingsFormGroup(label: "Tab Cleanup:", controls: [cleanupContainer]))
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func cleanupChanged() {
        onCleanupPolicyChange(TabCleanupPolicy.fromPopupIndex(cleanupPopup.indexOfSelectedItem))
    }

    func updateSettings(showInactiveCloseButtons: Bool,
                        warnBeforeClosingLastTab: Bool,
                        cleanupPolicy: TabCleanupPolicy,
                        extendPageColor: Bool) {
        for card in cards {
            card.setSelected(card.mode == SettingsStore.shared.layoutMode, animated: false)
        }
        showInactiveCloseButtonsCheck.isChecked = showInactiveCloseButtons
        warnBeforeClosingLastTabCheck.isChecked = warnBeforeClosingLastTab
        extendPageColorCheck.isChecked = extendPageColor
        cleanupPopup.selectItem(at: cleanupPolicy.popupIndex)
    }
}

/// A miniature window card illustrating Kitae's layout modes with traffic lights and tabs.
final class LayoutPreviewCard: FlippedView {
    let mode: ChromeRootView.LayoutMode
    var onSelect: (() -> Void)?
    private var selected = false
    private let titleLabel = NSTextField(labelWithString: "")
    private let miniWindow = FlippedView()

    init(mode: ChromeRootView.LayoutMode) {
        self.mode = mode
        super.init(frame: NSRect(x: 0, y: 0, width: 140, height: 110))

        miniWindow.wantsLayer = true
        miniWindow.layer?.cornerRadius = 8
        miniWindow.layer?.masksToBounds = true
        addSubview(miniWindow)

        titleLabel.stringValue = (mode == .topBar) ? "Top of Window" : "Sidebar"
        titleLabel.alignment = .center
        titleLabel.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        titleLabel.textColor = Theme.chromeText
        addSubview(titleLabel)

        refresh()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func setSelected(_ value: Bool, animated: Bool) {
        selected = value
        refresh()
    }

    private func refresh() {
        miniWindow.layer?.borderWidth = selected ? 2.0 : 1.0
        miniWindow.layer?.borderColor = Theme.cgColor(
            of: selected ? Theme.accent : Theme.dynamic(NSColor(white: 0, alpha: 0.15), NSColor(white: 1, alpha: 0.15)),
            for: effectiveAppearance
        )
        miniWindow.layer?.backgroundColor = Theme.cgColor(
            of: Theme.dynamic(NSColor(white: 0.95, alpha: 1.0), NSColor(red: 0.18, green: 0.18, blue: 0.20, alpha: 1.0)),
            for: effectiveAppearance
        )
        titleLabel.textColor = selected ? Theme.accent : Theme.chromeText
        titleLabel.font = NSFont.systemFont(ofSize: 12, weight: selected ? .semibold : .medium)
        miniWindow.needsDisplay = true
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    override func layout() {
        super.layout()
        miniWindow.frame = NSRect(x: 0, y: 0, width: bounds.width, height: 84)
        titleLabel.frame = NSRect(x: 0, y: 88, width: bounds.width, height: 18)
    }

    override func layoutSubtreeIfNeeded() {
        super.layoutSubtreeIfNeeded()
        setupMiniWindowContents()
    }

    private func setupMiniWindowContents() {
        miniWindow.subviews.forEach { $0.removeFromSuperview() }

        // Traffic lights (red, yellow, green)
        let red = NSView(frame: NSRect(x: 8, y: 8, width: 5, height: 5))
        red.wantsLayer = true; red.layer?.cornerRadius = 2.5
        red.layer?.backgroundColor = NSColor(red: 0.98, green: 0.36, blue: 0.34, alpha: 1).cgColor

        let yellow = NSView(frame: NSRect(x: 16, y: 8, width: 5, height: 5))
        yellow.wantsLayer = true; yellow.layer?.cornerRadius = 2.5
        yellow.layer?.backgroundColor = NSColor(red: 0.98, green: 0.74, blue: 0.22, alpha: 1).cgColor

        let green = NSView(frame: NSRect(x: 24, y: 8, width: 5, height: 5))
        green.wantsLayer = true; green.layer?.cornerRadius = 2.5
        green.layer?.backgroundColor = NSColor(red: 0.19, green: 0.78, blue: 0.35, alpha: 1).cgColor

        miniWindow.addSubview(red)
        miniWindow.addSubview(yellow)
        miniWindow.addSubview(green)

        if mode == .topBar {
            // Horizontal tab bar pills
            for i in 0..<3 {
                let tab = NSView(frame: NSRect(x: 36 + CGFloat(i) * 26, y: 7, width: 22, height: 7))
                tab.wantsLayer = true; tab.layer?.cornerRadius = 2.5
                tab.layer?.backgroundColor = (i == 0)
                    ? Theme.accent.withAlphaComponent(0.85).cgColor
                    : Theme.dynamic(NSColor(white: 0, alpha: 0.15), NSColor(white: 1, alpha: 0.2)).cgColor
                miniWindow.addSubview(tab)
            }
            // Address bar
            let addr = NSView(frame: NSRect(x: 10, y: 19, width: miniWindow.bounds.width - 20, height: 8))
            addr.wantsLayer = true; addr.layer?.cornerRadius = 3
            addr.layer?.backgroundColor = Theme.dynamic(NSColor(white: 0, alpha: 0.10), NSColor(white: 1, alpha: 0.15)).cgColor
            miniWindow.addSubview(addr)

            // Content card
            let card = NSView(frame: NSRect(x: 10, y: 31, width: miniWindow.bounds.width - 20, height: 46))
            card.wantsLayer = true; card.layer?.cornerRadius = 4
            card.layer?.backgroundColor = Theme.dynamic(NSColor.white, NSColor(red: 0.12, green: 0.12, blue: 0.14, alpha: 1.0)).cgColor
            miniWindow.addSubview(card)
        } else {
            // Vertical sidebar on left
            let rail = NSView(frame: NSRect(x: 0, y: 0, width: 34, height: 84))
            rail.wantsLayer = true
            rail.layer?.backgroundColor = Theme.dynamic(NSColor(white: 0.90, alpha: 1), NSColor(white: 0.14, alpha: 1)).cgColor
            miniWindow.addSubview(rail)
            miniWindow.addSubview(red); miniWindow.addSubview(yellow); miniWindow.addSubview(green)

            // 3 vertical tab pills
            for i in 0..<3 {
                let tab = NSView(frame: NSRect(x: 6, y: 20 + CGFloat(i) * 11, width: 22, height: 6))
                tab.wantsLayer = true; tab.layer?.cornerRadius = 2
                tab.layer?.backgroundColor = (i == 0)
                    ? Theme.accent.withAlphaComponent(0.85).cgColor
                    : Theme.dynamic(NSColor(white: 0, alpha: 0.15), NSColor(white: 1, alpha: 0.2)).cgColor
                rail.addSubview(tab)
            }

            // Top address bar on right
            let addr = NSView(frame: NSRect(x: 40, y: 8, width: miniWindow.bounds.width - 46, height: 8))
            addr.wantsLayer = true; addr.layer?.cornerRadius = 3
            addr.layer?.backgroundColor = Theme.dynamic(NSColor(white: 0, alpha: 0.10), NSColor(white: 1, alpha: 0.15)).cgColor
            miniWindow.addSubview(addr)

            // Main content card
            let card = NSView(frame: NSRect(x: 40, y: 20, width: miniWindow.bounds.width - 46, height: 57))
            card.wantsLayer = true; card.layer?.cornerRadius = 4
            card.layer?.backgroundColor = Theme.dynamic(NSColor.white, NSColor(red: 0.12, green: 0.12, blue: 0.14, alpha: 1.0)).cgColor
            miniWindow.addSubview(card)
        }
    }

    override func mouseDown(with event: NSEvent) {
        onSelect?()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Appearance Pane

enum AppearanceMode: String {
    case system, light, dark
}

final class AppearancePane: SettingsPane {
    private var radios: [RadioRowView] = []
    private let motionCheck = CheckRowView(title: "Prefer reduced motion")
    private let gpuCheck = CheckRowView(title: "Render with GPU (Metal acceleration)")
    private let smoothScrollCheck = CheckRowView(title: "Smooth page scrolling")

    var onAppearanceChange: (AppearanceMode) -> Void = { _ in }
    var onMotionReducedChange: (Bool) -> Void = { _ in }
    var onGpuChange: (Bool) -> Void = { _ in }
    var onSmoothScrollingChange: (Bool) -> Void = { _ in }

    init(current: AppearanceMode, motionReduced: Bool, gpuEnabled: Bool,
         smoothScrolling: Bool) {
        super.init(frame: .zero)

        let modes: [(String, AppearanceMode)] = [
            ("Follow System (Auto light/dark)", .system),
            ("Light", .light),
            ("Dark", .dark),
        ]
        for (title, mode) in modes {
            let row = RadioRowView(title: title, selected: mode == current)
            row.frame.size.height = 22
            row.onChange = { [weak self] in
                guard let self else { return }
                for other in self.radios { other.setSelected(false) }
                row.setSelected(true)
                self.onAppearanceChange(mode)
            }
            radios.append(row)
        }
        addGroup(SettingsFormGroup(label: "Theme:", controls: radios, rowSpacing: 8))

        motionCheck.isChecked = motionReduced
        motionCheck.frame.size.height = 22
        motionCheck.onChange = { [weak self] on in self?.onMotionReducedChange(on) }
        addGroup(SettingsFormGroup(label: "Motion:", controls: [motionCheck]))

        gpuCheck.isChecked = gpuEnabled
        gpuCheck.frame.size.height = 22
        gpuCheck.onChange = { [weak self] on in self?.onGpuChange(on) }

        smoothScrollCheck.isChecked = smoothScrolling
        smoothScrollCheck.frame.size.height = 22
        smoothScrollCheck.onChange = { [weak self] on in self?.onSmoothScrollingChange(on) }

        addGroup(SettingsFormGroup(label: "Rendering:", controls: [gpuCheck, smoothScrollCheck], rowSpacing: 8))
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func updateSettings(current: AppearanceMode, motionReduced: Bool,
                        gpuEnabled: Bool, smoothScrolling: Bool) {
        for (row, mode) in zip(radios, [AppearanceMode.system, .light, .dark]) {
            row.setSelected(mode == current)
        }
        motionCheck.isChecked = motionReduced
        gpuCheck.isChecked = gpuEnabled
        smoothScrollCheck.isChecked = smoothScrolling
    }
}

// MARK: - Privacy Pane (Pure Functional Options)

final class PrivacyPane: SettingsPane {
    private let httpsOnlyCheck = CheckRowView(title: "Always use secure HTTPS connections")
    private let warnHTTP = CheckRowView(title: "Warn before opening insecure HTTP links")
    private let blockThirdPartyCookiesCheck = CheckRowView(title: "Block third-party cookies")
    private let dntCheck = CheckRowView(title: "Send \"Do Not Track\" request header")
    private let clearHistoryCheck = CheckRowView(title: "Automatically clear history when quitting Kitae")

    var onClearHistoryOnQuitChange: (Bool) -> Void = { _ in }
    var onHTTPSOnlyChange: (Bool) -> Void = { _ in }
    var onWarnInsecureHTTPChange: (Bool) -> Void = { _ in }
    var onBlockThirdPartyCookiesChange: (Bool) -> Void = { _ in }
    var onDNTChange: (Bool) -> Void = { _ in }
    var onClearHistory: (() -> Void)?
    var onClearPageCache: (() -> Void)?

    init(clearHistoryOnQuit: Bool, httpsOnly: Bool, warnInsecureHTTP: Bool,
         blockThirdPartyCookies: Bool, sendDNT: Bool) {
        super.init(frame: .zero)

        httpsOnlyCheck.isChecked = httpsOnly
        warnHTTP.isChecked = warnInsecureHTTP
        blockThirdPartyCookiesCheck.isChecked = blockThirdPartyCookies
        dntCheck.isChecked = sendDNT
        clearHistoryCheck.isChecked = clearHistoryOnQuit
        for row in [httpsOnlyCheck, warnHTTP, blockThirdPartyCookiesCheck,
                    dntCheck, clearHistoryCheck] {
            row.frame.size.height = 22
        }
        httpsOnlyCheck.onChange = { [weak self] on in self?.onHTTPSOnlyChange(on) }
        warnHTTP.onChange = { [weak self] on in self?.onWarnInsecureHTTPChange(on) }
        blockThirdPartyCookiesCheck.onChange = { [weak self] on in
            self?.onBlockThirdPartyCookiesChange(on)
        }
        dntCheck.onChange = { [weak self] on in self?.onDNTChange(on) }
        clearHistoryCheck.onChange = { [weak self] on in
            self?.onClearHistoryOnQuitChange(on)
        }

        addGroup(SettingsFormGroup(label: "Security:",
                                   controls: [httpsOnlyCheck, warnHTTP],
                                   rowSpacing: 8))
        addGroup(SettingsFormGroup(label: "Privacy:",
                                   controls: [blockThirdPartyCookiesCheck, dntCheck],
                                   rowSpacing: 8))

        let buttonsContainer = FlippedView(frame: NSRect(x: 0, y: 0, width: 440, height: 30))
        let clearHistoryBtn = NSButton(title: "Clear Browsing History", target: self, action: #selector(clearHistoryClicked))
        clearHistoryBtn.bezelStyle = .rounded
        clearHistoryBtn.font = NSFont.systemFont(ofSize: 13)
        clearHistoryBtn.frame = NSRect(x: 0, y: 0, width: 175, height: 28)
        let clearCacheBtn = NSButton(title: "Clear Page Cache", target: self, action: #selector(clearCacheClicked))
        clearCacheBtn.bezelStyle = .rounded
        clearCacheBtn.font = NSFont.systemFont(ofSize: 13)
        clearCacheBtn.frame = NSRect(x: 185, y: 0, width: 150, height: 28)
        buttonsContainer.addSubview(clearHistoryBtn)
        buttonsContainer.addSubview(clearCacheBtn)
        addGroup(SettingsFormGroup(label: "Data & Storage:",
                                   controls: [clearHistoryCheck, buttonsContainer],
                                   rowSpacing: 10))
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func clearHistoryClicked() {
        onClearHistory?()
        let alert = NSAlert()
        alert.messageText = "Browsing History Cleared"
        alert.informativeText = "All stored history entries have been successfully erased."
        alert.alertStyle = .informational
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    @objc private func clearCacheClicked() {
        onClearPageCache?()
        let alert = NSAlert()
        alert.messageText = "Page Cache Cleared"
        alert.informativeText = "Rendered frames, page caches, and image caches have been cleared."
        alert.alertStyle = .informational
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    func updateSettings(clearHistoryOnQuit: Bool, httpsOnly: Bool,
                        warnInsecureHTTP: Bool, blockThirdPartyCookies: Bool,
                        sendDNT: Bool) {
        clearHistoryCheck.isChecked = clearHistoryOnQuit
        httpsOnlyCheck.isChecked = httpsOnly
        warnHTTP.isChecked = warnInsecureHTTP
        blockThirdPartyCookiesCheck.isChecked = blockThirdPartyCookies
        dntCheck.isChecked = sendDNT
    }
}

// MARK: - Advanced Pane (Functional Settings)

final class AdvancedPane: SettingsPane {
    private let fullUrlCheck = CheckRowView(title: "Always show full website URL")
    private let highlightDomainCheck = CheckRowView(title: "Highlight top-level domain in address field")
    private let debugBoundsCheck = CheckRowView(title: "Show layout and render bounding boxes")
    private let searchPopup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 220, height: 26), pullsDown: false)
    private let diagnosticsButton: NSButton

    var onAlwaysShowFullURLChange: (Bool) -> Void = { _ in }
    var onHighlightTopLevelDomainChange: (Bool) -> Void = { _ in }
    var onShowDebugBoundsChange: (Bool) -> Void = { _ in }
    var onSearchProviderChange: (SearchProviderSetting) -> Void = { _ in }
    var onShowDiagnostics: (() -> Void)?
    var onReset: (() -> Void)?

    init(alwaysShowFullURL: Bool, searchProvider: SearchProviderSetting,
         highlightTopLevelDomain: Bool, showDebugBounds: Bool) {
        diagnosticsButton = NSButton(title: "Copy Diagnostics Report", target: nil, action: nil)
        super.init(frame: .zero)

        fullUrlCheck.isChecked = alwaysShowFullURL
        fullUrlCheck.frame.size.height = 22
        fullUrlCheck.onChange = { [weak self] on in
            self?.onAlwaysShowFullURLChange(on)
        }
        highlightDomainCheck.isChecked = highlightTopLevelDomain
        highlightDomainCheck.frame.size.height = 22
        highlightDomainCheck.onChange = { [weak self] on in
            self?.onHighlightTopLevelDomainChange(on)
        }
        addGroup(SettingsFormGroup(label: "Address Bar:",
                                   controls: [fullUrlCheck, highlightDomainCheck],
                                   rowSpacing: 8))

        searchPopup.addItems(withTitles: SearchProviderSetting.allCases.map(\.title))
        searchPopup.target = self
        searchPopup.action = #selector(searchProviderChanged)
        searchPopup.selectItem(at: searchProvider.rawValue)
        addGroup(SettingsFormGroup(label: "Search Engine:", controls: [searchPopup]))

        debugBoundsCheck.isChecked = showDebugBounds
        debugBoundsCheck.frame.size.height = 22
        debugBoundsCheck.onChange = { [weak self] on in
            self?.onShowDebugBoundsChange(on)
        }
        diagnosticsButton.target = self
        diagnosticsButton.action = #selector(showDiagnosticsClicked)
        diagnosticsButton.bezelStyle = .rounded
        diagnosticsButton.frame = NSRect(x: 0, y: 0, width: 220, height: 28)
        addGroup(SettingsFormGroup(label: "Debug:",
                                   controls: [debugBoundsCheck, diagnosticsButton],
                                   rowSpacing: 8))

        let resetBtn = NSButton(title: "Reset All Settings to Defaults", target: self, action: #selector(resetSettingsClicked))
        resetBtn.bezelStyle = .rounded
        resetBtn.font = NSFont.systemFont(ofSize: 13)
        resetBtn.frame = NSRect(x: 0, y: 0, width: 220, height: 28)

        let resetContainer = FlippedView(frame: NSRect(x: 0, y: 0, width: 440, height: 28))
        resetContainer.addSubview(resetBtn)
        addGroup(SettingsFormGroup(label: "Reset:", controls: [resetContainer]))
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    @objc private func searchProviderChanged() {
        guard let provider = SearchProviderSetting(rawValue: searchPopup.indexOfSelectedItem) else { return }
        onSearchProviderChange(provider)
    }

    @objc private func showDiagnosticsClicked() {
        onShowDiagnostics?()
    }

    @objc private func resetSettingsClicked() {
        let alert = NSAlert()
        alert.messageText = "Reset Settings"
        alert.informativeText = "Are you sure you want to reset all preferences to their default values?"
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Reset")
        if alert.runModal() == .alertSecondButtonReturn {
            onReset?()
        }
    }

    func updateSettings(alwaysShowFullURL: Bool, searchProvider: SearchProviderSetting,
                        highlightTopLevelDomain: Bool, showDebugBounds: Bool) {
        fullUrlCheck.isChecked = alwaysShowFullURL
        highlightDomainCheck.isChecked = highlightTopLevelDomain
        debugBoundsCheck.isChecked = showDebugBounds
        searchPopup.selectItem(at: searchProvider.rawValue)
    }
}

// MARK: - About Pane (Beautifully Organized & Aligned)

final class AboutPane: SettingsPane {
    init() {
        super.init(frame: .zero)

        let cardContainer = FlippedView(frame: NSRect(x: 0, y: 0, width: 450, height: 180))

        let iconBadge = NSView(frame: NSRect(x: 0, y: 2, width: 56, height: 56))
        iconBadge.wantsLayer = true
        iconBadge.layer?.cornerRadius = 13
        iconBadge.layer?.backgroundColor = Theme.accent.cgColor

        let kGlyph = NSTextField(labelWithString: "K")
        kGlyph.font = NSFont.systemFont(ofSize: 34, weight: .black)
        kGlyph.textColor = .white
        kGlyph.alignment = .center
        kGlyph.frame = NSRect(x: 0, y: 8, width: 56, height: 42)
        iconBadge.addSubview(kGlyph)
        cardContainer.addSubview(iconBadge)

        let name = NSTextField(labelWithString: "Kitae")
        name.font = NSFont.systemFont(ofSize: 24, weight: .bold)
        name.textColor = Theme.chromeText
        name.frame = NSRect(x: 70, y: 4, width: 300, height: 28)
        cardContainer.addSubview(name)

        let version = NSTextField(labelWithString: AppVersion.display)
        version.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        version.textColor = Theme.chromeMutedText
        version.frame = NSRect(x: 70, y: 32, width: 300, height: 16)
        cardContainer.addSubview(version)

        let tagline = NSTextField(labelWithString: "The web, without the noise.")
        tagline.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        tagline.textColor = Theme.accent
        tagline.frame = NSRect(x: 0, y: 76, width: 420, height: 18)
        cardContainer.addSubview(tagline)

        let desc = NSTextField(wrappingLabelWithString: """
        A lightweight reader browser designed for speed and focused reading. \
        Built natively for macOS with a high-performance Rust core (kitae_core) \
        and hardware-accelerated Metal typography.
        """)
        desc.font = NSFont.systemFont(ofSize: 13, weight: .regular)
        desc.textColor = Theme.chromeMutedText
        desc.frame = NSRect(x: 0, y: 102, width: 420, height: 50)
        cardContainer.addSubview(desc)

        addGroup(SettingsFormGroup(label: "About:", controls: [cardContainer]))
    }

    required init?(coder: NSCoder) { fatalError("not used") }
}

// MARK: - Reusable Row Components

final class RadioRowView: FlippedView {
    private let circle = NSView()
    private let dot = NSView()
    private let titleField = NSTextField(labelWithString: "")
    private var selected = false
    var onChange: (() -> Void)?

    init(title: String, selected: Bool) {
        titleField.stringValue = title
        titleField.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        titleField.textColor = Theme.chromeText

        super.init(frame: .zero)
        self.selected = selected

        circle.wantsLayer = true
        circle.layer?.cornerRadius = 7
        circle.layer?.borderWidth = 1.5

        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        dot.layer?.backgroundColor = NSColor.white.cgColor
        dot.isHidden = true

        addSubview(circle)
        addSubview(dot)
        addSubview(titleField)
        refresh()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func setSelected(_ value: Bool) {
        selected = value
        refresh()
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setAnimationDuration(Theme.motionEnabled ? Theme.animationDuration : 0)
        circle.layer?.borderColor = Theme.cgColor(of: selected ? Theme.accent : Theme.addressFieldBorder,
                                                 for: effectiveAppearance)
        circle.layer?.backgroundColor = Theme.cgColor(
            of: selected ? Theme.accent : Theme.addressFieldBackground, for: effectiveAppearance)
        CATransaction.commit()
        dot.isHidden = !selected
        titleField.textColor = selected ? Theme.accent : Theme.chromeText
    }

    override func layout() {
        super.layout()
        circle.frame = NSRect(x: 0, y: (bounds.height - 14) / 2, width: 14, height: 14)
        dot.frame = NSRect(x: 4, y: bounds.height / 2 - 3, width: 6, height: 6)
        titleField.frame = NSRect(x: 22, y: (bounds.height - 17) / 2, width: bounds.width - 24, height: 17)
    }

    override func mouseDown(with event: NSEvent) {
        onChange?()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

final class CheckRowView: FlippedView {
    private let box = Checkbox()
    private let titleField = NSTextField(labelWithString: "")
    var onChange: ((Bool) -> Void)?

    var isChecked: Bool {
        get { box.isChecked }
        set { box.isChecked = newValue }
    }

    init(title: String) {
        titleField.stringValue = title
        titleField.font = NSFont.systemFont(ofSize: 13, weight: .medium)
        titleField.textColor = Theme.chromeText

        super.init(frame: .zero)
        addSubview(box)
        addSubview(titleField)
        box.setAccessibilityLabel(title)

        box.onChange = { [weak self] on in self?.onChange?(on) }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func layout() {
        super.layout()
        box.frame = NSRect(x: 0, y: (bounds.height - 16) / 2, width: 16, height: 16)
        titleField.frame = NSRect(x: 24, y: (bounds.height - 17) / 2, width: bounds.width - 26, height: 17)
    }

    override func mouseDown(with event: NSEvent) {
        box.setChecked(!box.isChecked, animated: true)
        onChange?(box.isChecked)
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
