// AppDelegate.swift — the NSApplication side of Kitae: window, menu bar,
// chrome wiring, keyboard shortcuts, appearance tracking, the ⌘K command
// palette, and the Settings window.
//
// The engine never touches AppKit: frames arrive here decoded and the app
// decides what to do with them. Escape-hatch URLs go to NSWorkspace.

import AppKit
import UniformTypeIdentifiers

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate,
    TabManagerDelegate, EngineDelegate {

    // MARK: Windows & chrome

    private var window: NSWindow!
    private var chrome: ChromeRootView!
    private var toolbar: ToolbarView!
    private var tabStrip: TabStripView!
    private var sidebar: SidebarView!

    // MARK: Tabs & state

    private let tabManager = TabManager()
    private var layoutMode: ChromeRootView.LayoutMode = .topBar
    private var statusHint: String?
    private let palette = CommandPalette()
    private var settingsController: SettingsWindowController?
    /// H5: the read-only inspector panel (created on first use).
    private var inspectorPanel: InspectorPanel?
    /// B4: the agent panel — activity, prompt and triggers for the verbs
    /// running on the active tab (created on first use).
    private var agentPanel: AgentPanel?
    /// QTSQ audio transport, docked below the card when the active page
    /// publishes an audio MediaInfo (the engine decodes to a temp CAF; this
    /// view plays it and mirrors transport back through command 16).
    private var mediaPlayerView: QTSQAudioPlayerView?
    /// QTSQ video transport (J5): hosts play/pause + scrubber; the engine
    /// paints the stepped frame into the page, the bar drives the clock.
    private var videoPlayerView: QTSQVideoPlayerView?
    /// URL of the page the player view is currently attached to.
    private var mediaPlayerURL = ""
    private var mediaPlayerTabID: UUID?
    /// Files Finder/Open panel handed us before the window existed; drained
    /// in applicationDidFinishLaunching.
    private var pendingOpenURLs: [URL] = []
    private var pendingDeepLinks: [DeepLinkTarget] = []
    /// Keeps the share picker's delegate alive while the menu tracks.
    private var sharePickerDelegate: SharePickerDelegate?

    /// Window resizes are fanned out to every engine only after a short
    /// quiet period so a live drag doesn't spam KITAE_CMD_RESIZE.
    private var resizeDebounce: Timer?

    private var defaultLayoutMode: ChromeRootView.LayoutMode {
        SettingsStore.shared.layoutMode
    }

    private var appearanceMode: AppearanceMode {
        SettingsStore.shared.appearanceMode
    }

    /// The environment pages see through `@media (prefers-…)`: a dark scheme
    /// when the app setting (or the OS, in system mode) is dark, plus the
    /// reduce-motion toggle. Pushed to every engine on change (Q6).
    private var pageEnvironmentIsDark: Bool {
        switch appearanceMode {
        case .light: return false
        case .dark: return true
        case .system:
            return NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        }
    }

    private func pushEnvironmentToEngines() {
        tabManager.setEnvironment(
            dark: pageEnvironmentIsDark,
            reducedMotion: SettingsStore.shared.motionReduced)
    }

    private func pushPrivacySettingsToEngines() {
        tabManager.setPrivacyPolicy(
            dnt: SettingsStore.shared.sendDNT,
            blockThirdPartyCookies: SettingsStore.shared.blockThirdPartyCookies,
            httpsOnly: SettingsStore.shared.httpsOnly,
            warnInsecureHTTP: SettingsStore.shared.warnInsecureHTTP
        )
        tabManager.setSearchProvider(SettingsStore.shared.searchProvider)
    }

    // MARK: Application lifecycle

    func applicationDidFinishLaunching(_ notification: Notification) {
        HstsStore.shared.restore()
        buildWindow()
        buildMenus()
        applyAppearanceMode()
        // Follow OS appearance flips while "system" mode is selected so
        // pages restyle without a restart. (The SDK's Swift overlay doesn't
        // type this notification; the raw name is the public constant.)
        NotificationCenter.default.addObserver(
            forName: NSNotification.Name("NSApplicationDidChangeEffectiveAppearanceNotification"),
            object: NSApp, queue: .main) { [weak self] _ in
            self?.pushEnvironmentToEngines()
        }

        layoutMode = defaultLayoutMode
        chrome.apply(mode: layoutMode, animated: false)

        tabManager.delegate = self
        tabManager.setTabCleanupPolicy(SettingsStore.shared.tabCleanupPolicy)
        tabManager.setShowDebugBounds(SettingsStore.shared.showDebugBounds)
        toolbar.addressField.alwaysShowFullURL = SettingsStore.shared.alwaysShowFullURL
        toolbar.addressField.highlightTopLevelDomain = SettingsStore.shared.highlightTopLevelDomain
        // H7: restore the previous session (falls back to a fresh start tab).
        if !tabManager.restoreSession() {
            tabManager.newTab()
        }
        tabManager.start()
        pushPrivacySettingsToEngines()

        window.makeKeyAndOrderFront(nil)
        window.center()
        if !window.isZoomed {
            window.zoom(nil)
        }
        pushResizeToEngines()
        pushEnvironmentToEngines()

        // Files queued by open-while-launching Apple Events, then whatever
        // the command line carried (dev launches: ./Kitae file:///…).
        for target in pendingDeepLinks { openDeepLink(target) }
        pendingDeepLinks = []
        for url in pendingOpenURLs { openFileURL(url) }
        pendingOpenURLs = []
        if let target = Self.launchTarget(from: CommandLine.arguments) {
            openLaunchTarget(target)
        }
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        tabManager.performInactiveTabCleanup()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        // H7: persist the session synchronously before the engines die.
        let snap = tabManager.sessionSnapshot()
        SessionStore.shared.saveNow(urls: snap.urls, activeIndex: snap.activeIndex)
        HstsStore.shared.saveNow()
        if SettingsStore.shared.clearHistoryOnQuit {
            HistoryManager.shared.clearAllNow()
        }
        return .terminateNow
    }

    // MARK: Finder / Open-panel file opening (J6)

    /// Finder double-click or Open With: navigate the active tab to the
    /// file's file:// URL (the engine sniffs .qtsq and friends by content).
    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            if let target = DeepLinkParser.parse(url) {
                queueOrOpenDeepLink(target)
            } else if url.isFileURL {
                queueOrOpen(url)
            }
        }
    }

    /// Legacy target/action hook for hosts that still send `openFile:`.
    @objc func openFile(_ sender: Any?) {
        guard let url = sender as? URL ?? (sender as? NSString).map({ URL(fileURLWithPath: $0 as String) })
        else { return }
        queueOrOpen(url)
    }

    /// Before the window exists, opening must wait; after, navigate now.
    private func queueOrOpen(_ url: URL) {
        if window != nil {
            openFileURL(url)
        } else {
            pendingOpenURLs.append(url)
        }
    }

    private func queueOrOpenDeepLink(_ target: DeepLinkTarget) {
        if window != nil {
            openDeepLink(target)
        } else {
            pendingDeepLinks.append(target)
        }
    }

    private func openDeepLink(_ target: DeepLinkTarget) {
        switch target {
        case .history:
            tabManager.navigateActive("kitae://history")
        case let .open(url, newTab):
            if newTab { tabManager.newTab() }
            tabManager.navigateActive(url)
        }
    }

    private func openFileURL(_ url: URL) {
        let path = url.path
        guard !path.isEmpty else { return }
        let encoded = path.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? path
        tabManager.navigateActive("file://\(encoded)")
    }

    /// The command line (./Kitae file:///… or ./Kitae /path/to/file.qtsq).
    /// Skips process flags like -psn_* and the `open --args` marker.
    private static func launchTarget(from args: [String]) -> String? {
        for arg in args.dropFirst() where !arg.isEmpty && !arg.hasPrefix("-") {
            return arg
        }
        return nil
    }

    private func openLaunchTarget(_ target: String) {
        if let deepLink = DeepLinkParser.parse(target) {
            openDeepLink(deepLink)
            return
        }
        if target.hasPrefix("file://") || target.hasPrefix("http://")
            || target.hasPrefix("https://") || target.hasPrefix("about:") {
            tabManager.navigateActive(target)
            return
        }
        openFileURL(URL(fileURLWithPath: (target as NSString).expandingTildeInPath))
    }

    func windowWillClose(_ notification: Notification) {
        tabManager.stop()
    }

    // MARK: Window construction

    private func buildWindow() {
        let size = NSSize(width: 1160, height: 760)
        let rect = NSRect(
            x: 0, y: 0,
            width: size.width, height: size.height)

        window = NSWindow(
            contentRect: rect,
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false)
        window.title = "Kitae"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.minSize = NSSize(width: 720, height: 480)
        window.delegate = self
        window.isOpaque = false
        window.hasShadow = true
        window.backgroundColor = .clear
        window.isReleasedWhenClosed = false

        toolbar = ToolbarView(addressPlaceholder: "Search or enter address")
        tabStrip = TabStripView(frame: .zero)
        sidebar = SidebarView(frame: .zero)

        chrome = ChromeRootView(toolbar: toolbar, tabStrip: tabStrip, sidebar: sidebar)
        chrome.autoresizingMask = [.width, .height]
        chrome.frame = NSRect(origin: .zero, size: size)
        window.contentView = chrome

        // Toolbar wiring
        toolbar.backButton.action = { [weak self] in self?.tabManager.goBack() }
        toolbar.forwardButton.action = { [weak self] in self?.tabManager.goForward() }
        toolbar.reloadButton.action = { [weak self] in
            self?.toolbar.addressPill.setLoading(true)
            self?.tabManager.reload()
        }
        toolbar.addressField.onSubmit = { [weak self] input in
            self?.toolbar.addressPill.setLoading(true)
            self?.tabManager.navigateActive(input)
        }
        toolbar.shareButton.action = { [weak self] in self?.performShare() }
        toolbar.menuButton.action = { [weak self] in self?.chrome.toggleMenuDrawer() }
        toolbar.sidebarButton.action = { [weak self] in self?.chrome.toggleSidebar() }
        toolbar.downloadsButton.action = { [weak self] in
            // H1: toggle the downloads panel anchored to the toolbar button.
            guard let self = self else { return }
            DownloadsPopover.shared.toggle(relativeTo: self.toolbar.downloadsButton)
        }
        toolbar.onOpenSettings = { [weak self] in self?.showSettings(nil) }

        // H1: keep the toolbar badge in sync with the download ledger.
        NotificationCenter.default.addObserver(
            forName: DownloadCenter.didChangeNotification,
            object: nil, queue: .main) { [weak self] _ in
            guard let self = self else { return }
            self.toolbar.setDownloadBadge(count: DownloadCenter.shared.activeCount)
        }

        // Menu drawer wiring
        chrome.menuDrawer.onOpenSettings = { [weak self] in self?.showSettings(nil) }
        chrome.menuDrawer.onOpenFullHistory = { [weak self] in self?.showFullHistory(nil) }
        chrome.menuDrawer.onNavigate = { [weak self] url in self?.tabManager.navigateActive(url) }
        chrome.onContentCardResized = { [weak self] in self?.pushResizeToEngines() }

        // Tab strip wiring (top-bar mode)
        tabStrip.onSelect = { [weak self] index in self?.tabManager.activate(index: index) }
        tabStrip.onClose = { [weak self] index in self?.tabManager.requestCloseTab(at: index) }
        tabStrip.onNewTab = { [weak self] in self?.tabManager.newTab() }

        // Sidebar wiring (sidebar mode)
        sidebar.onSelect = { [weak self] index in self?.tabManager.activate(index: index) }
        sidebar.onClose = { [weak self] index in self?.tabManager.requestCloseTab(at: index) }
        sidebar.onNewTab = { [weak self] in self?.tabManager.newTab() }
        sidebar.onToggle = { [weak self] in self?.chrome.toggleSidebar() }
        chrome.onSidebarVisibilityChanged = { [weak self] isVisible in
            self?.setTrafficLightsHidden(!isVisible)
        }

        // ⌘K tab switcher
        palette.onSwitchToTab = { [weak self] index in
            self?.tabManager.activate(index: index)
        }
    }

    // MARK: Menu bar

    private func buildMenus() {
        let mainMenu = NSMenu()

        // App menu (Kitae)
        let appMenuItem = NSMenuItem()
        mainMenu.addItem(appMenuItem)
        let appMenu = NSMenu()
        appMenuItem.submenu = appMenu
        appMenu.addItem(withTitle: "About Kitae",
                        action: #selector(showAbout(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Settings…",
                        action: #selector(showSettings(_:)), keyEquivalent: ",")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Hide Kitae",
                        action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        let hideOthers = appMenu.addItem(withTitle: "Hide Others",
                                         action: #selector(NSApplication.hideOtherApplications(_:)),
                                         keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(withTitle: "Show All",
                        action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Kitae",
                        action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        // File
        let fileItem = NSMenuItem()
        mainMenu.addItem(fileItem)
        let fileMenu = NSMenu(title: "File")
        fileItem.submenu = fileMenu
        fileMenu.addItem(withTitle: "New Tab",
                         action: #selector(newTab(_:)), keyEquivalent: "t")
        let reopen = fileMenu.addItem(withTitle: "Reopen Closed Tab",
                                      action: #selector(reopenClosedTab(_:)), keyEquivalent: "t")
        reopen.keyEquivalentModifierMask = [.command, .shift]
        let close = fileMenu.addItem(withTitle: "Close Tab",
                                     action: #selector(closeTab(_:)), keyEquivalent: "w")
        close.keyEquivalentModifierMask = [.command, .option]
        fileMenu.addItem(.separator())
        fileMenu.addItem(withTitle: "Share Page…",
                         action: #selector(shareCurrentPage(_:)), keyEquivalent: "i")
        fileMenu.addItem(withTitle: "Print Page…",
                         action: #selector(printCurrentPage(_:)), keyEquivalent: "p")

        // Edit (needed for copy/paste in the address field)
        let editItem = NSMenuItem()
        mainMenu.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editItem.submenu = editMenu
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        editMenu.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        editMenu.addItem(.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All",
                         action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")

        // View
        let viewItem = NSMenuItem()
        mainMenu.addItem(viewItem)
        let viewMenu = NSMenu(title: "View")
        viewItem.submenu = viewMenu
        viewMenu.addItem(withTitle: "Command Palette…",
                         action: #selector(showPalette(_:)), keyEquivalent: "k")
        viewMenu.addItem(withTitle: "Focus Address Bar",
                         action: #selector(focusAddressBar(_:)), keyEquivalent: "l")
        viewMenu.addItem(.separator())
        viewMenu.addItem(withTitle: "Reload Page",
                         action: #selector(reload(_:)), keyEquivalent: "r")
        viewMenu.addItem(.separator())
        let downloadsMenuItem = viewMenu.addItem(withTitle: "Downloads",
                                                 action: #selector(showDownloads(_:)),
                                                 keyEquivalent: "j")
        downloadsMenuItem.keyEquivalentModifierMask = [.command, .shift]
        viewMenu.addItem(.separator())
        viewMenu.addItem(withTitle: "Top Bar Layout",
                         action: #selector(useTopBar(_:)), keyEquivalent: "1")
        viewMenu.addItem(withTitle: "Sidebar Layout",
                         action: #selector(useSidebar(_:)), keyEquivalent: "2")
        viewMenu.addItem(withTitle: "Toggle Sidebar",
                         action: #selector(toggleSidebarAction(_:)), keyEquivalent: "s")
        let inspectMenuItem = viewMenu.addItem(withTitle: "Inspect Element",
                                               action: #selector(toggleInspector(_:)),
                                               keyEquivalent: "i")
        inspectMenuItem.keyEquivalentModifierMask = [.command, .option]
        let agentMenuItem = viewMenu.addItem(withTitle: "Agent Panel",
                                             action: #selector(toggleAgentPanel(_:)),
                                             keyEquivalent: "a")
        agentMenuItem.keyEquivalentModifierMask = [.command, .option]

        // History
        let historyItem = NSMenuItem()
        mainMenu.addItem(historyItem)
        let historyMenu = NSMenu(title: "History")
        historyItem.submenu = historyMenu
        historyMenu.addItem(withTitle: "Back",
                            action: #selector(goBack(_:)), keyEquivalent: "[")
        historyMenu.addItem(withTitle: "Forward",
                            action: #selector(goForward(_:)), keyEquivalent: "]")
        historyMenu.addItem(.separator())
        historyMenu.addItem(withTitle: "Show Full History",
                            action: #selector(showFullHistory(_:)), keyEquivalent: "y")
        historyMenu.addItem(withTitle: "Clear History…",
                            action: #selector(clearHistory(_:)), keyEquivalent: "")

        // Window
        let windowItem = NSMenuItem()
        mainMenu.addItem(windowItem)
        let windowMenu = NSMenu(title: "Window")
        windowItem.submenu = windowMenu
        windowMenu.addItem(withTitle: "Minimize",
                           action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom",
                           action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        windowMenu.addItem(withTitle: "Settings",
                           action: #selector(showSettings(_:)), keyEquivalent: "")

        NSApp.mainMenu = mainMenu
    }

    // MARK: Appearance

    private func applyAppearanceMode() {
        switch appearanceMode {
        case .system:
            NSApp.appearance = nil
        case .light:
            NSApp.appearance = NSAppearance(named: .aqua)
        case .dark:
            NSApp.appearance = NSAppearance(named: .darkAqua)
        }
    }

    // MARK: Downloads (H1)

    @objc func showDownloads(_ sender: Any?) {
        DownloadsPopover.shared.toggle(relativeTo: toolbar.downloadsButton)
    }

    // MARK: Command palette

    @objc func showPalette(_ sender: Any?) {
        guard let window else { return }
        if palette.isVisible {
            palette.dismiss()
            return
        }
        var tabItems: [(title: String, url: String, active: Bool)] = []
        for (index, tab) in tabManager.tabs.enumerated() {
            let title = tab.latestFrame?.title.isEmpty == false
                ? tab.latestFrame!.title : "New Tab"
            tabItems.append((title: title, url: tab.latestFrame?.url ?? "",
                             active: index == tabManager.activeIndex))
        }
        let actions: [(String, () -> Void)] = [
            ("New Tab", { [weak self] in self?.tabManager.newTab() }),
            ("Reload Page", { [weak self] in self?.tabManager.reload() }),
            ("Back", { [weak self] in self?.tabManager.goBack() }),
            ("Forward", { [weak self] in self?.tabManager.goForward() }),
            ("Print Page…", { [weak self] in self?.printCurrentPage(nil) }),
            ("Top Bar Layout", { [weak self] in self?.setLayoutMode(.topBar) }),
            ("Sidebar Layout", { [weak self] in self?.setLayoutMode(.sidebar) }),
            ("Settings…", { [weak self] in self?.showSettings(nil) }),
            ("Inspect Element", { [weak self] in self?.toggleInspector(nil) }),
            ("Agent Panel", { [weak self] in self?.toggleAgentPanel(nil) }),
            ("Reopen Closed Tab", { [weak self] in self?.reopenClosedTab(nil) }),
        ]
        palette.setSources(tabs: tabItems, actions: actions)
        palette.present(from: window)
    }

    // MARK: Share

    @objc func shareCurrentPage(_ sender: Any?) { performShare() }

    private func performShare() {
        guard let tab = tabManager.activeTab,
              let url = URL(string: tab.displayedURL), url.scheme != nil else { return }
        let picker = NSSharingServicePicker(items: [url])
        let delegate = SharePickerDelegate(page: tab)
        sharePickerDelegate = delegate  // the picker holds its delegate weakly
        picker.delegate = delegate
        picker.show(relativeTo: toolbar.shareButton.bounds,
                    of: toolbar.shareButton, preferredEdge: .minY)
    }

    // MARK: Print (H3)

    @objc func printCurrentPage(_ sender: Any?) {
        guard let tab = tabManager.activeTab else { return }
        PagePrinter.printActiveTab(tab, from: window) { [weak self] error in
            if let error {
                self?.activeTabStatusHint("Print failed: \(error)")
            } else {
                self?.activeTabStatusHint(nil)
            }
        }
    }

    // MARK: Settings window

    @objc func showSettings(_ sender: Any?) {
        let controller: SettingsWindowController
        if let existing = settingsController {
            controller = existing
        } else {
            let newController = SettingsWindowController(
                currentLayout: layoutMode,
                currentAppearance: appearanceMode,
                motionReduced: SettingsStore.shared.motionReduced,
                gpuEnabled: SettingsStore.shared.gpuEnabled)
            newController.onLayoutChange = { [weak self] mode in
                self?.setLayoutMode(mode)
            }
            newController.onAppearanceChange = { [weak self] mode in
                SettingsStore.shared.appearanceMode = mode
                self?.applyAppearanceMode()
                self?.pushEnvironmentToEngines()
            }
            newController.onMotionReducedChange = { [weak self] reduced in
                SettingsStore.shared.motionReduced = reduced
                Theme.motionReducedByUser = reduced
                self?.pushEnvironmentToEngines()
            }
            newController.onGpuChange = { [weak self] on in
                SettingsStore.shared.gpuEnabled = on
                self?.tabManager.setGPUEnabled(on)
            }
            newController.onSmoothScrollingChange = { [weak self] on in
                SettingsStore.shared.smoothPageScrolling = on
                self?.tabManager.setSmoothScrolling(on)
            }
            newController.onShowInactiveCloseButtonsChange = { [weak self] on in
                SettingsStore.shared.showInactiveTabCloseButtons = on
                self?.reloadTabLists()
            }
            newController.onExtendPageColorChange = { [weak self] on in
                SettingsStore.shared.extendPageColor = on
                self?.reloadTabLists()
            }
            newController.onWarnBeforeClosingLastTabChange = { on in
                SettingsStore.shared.warnBeforeClosingLastTab = on
            }
            newController.onTabCleanupPolicyChange = { [weak self] policy in
                SettingsStore.shared.tabCleanupPolicy = policy
                self?.tabManager.setTabCleanupPolicy(policy)
            }
            newController.onClearHistoryOnQuitChange = { on in
                SettingsStore.shared.clearHistoryOnQuit = on
            }
            newController.onHTTPSOnlyChange = { [weak self] on in
                SettingsStore.shared.httpsOnly = on
                self?.pushPrivacySettingsToEngines()
            }
            newController.onWarnInsecureHTTPChange = { [weak self] on in
                SettingsStore.shared.warnInsecureHTTP = on
                self?.pushPrivacySettingsToEngines()
            }
            newController.onBlockThirdPartyCookiesChange = { [weak self] on in
                SettingsStore.shared.blockThirdPartyCookies = on
                self?.pushPrivacySettingsToEngines()
            }
            newController.onDNTChange = { [weak self] on in
                SettingsStore.shared.sendDNT = on
                self?.pushPrivacySettingsToEngines()
            }
            newController.onSearchProviderChange = { [weak self] provider in
                SettingsStore.shared.searchProvider = provider
                self?.pushPrivacySettingsToEngines()
            }
            newController.onAlwaysShowFullURLChange = { [weak self] on in
                SettingsStore.shared.alwaysShowFullURL = on
                self?.toolbar.addressField.alwaysShowFullURL = on
                self?.toolbar.addressField.refreshURL()
            }
            newController.onHighlightTopLevelDomainChange = { [weak self] on in
                SettingsStore.shared.highlightTopLevelDomain = on
                self?.toolbar.addressField.highlightTopLevelDomain = on
                self?.toolbar.addressField.refreshURL()
            }
            newController.onShowDebugBoundsChange = { [weak self] on in
                SettingsStore.shared.showDebugBounds = on
                self?.tabManager.setShowDebugBounds(on)
            }
            newController.onShowDiagnostics = { [weak self] in
                self?.copyDiagnosticsReport()
            }
            newController.onClearHistory = {
                HistoryManager.shared.clearAllNow()
            }
            newController.onClearPageCache = { [weak self] in
                self?.tabManager.clearPageCaches()
            }
            newController.onReset = { [weak self] in
                self?.resetSettings()
            }
            settingsController = newController
            controller = newController
        }

        if let win = controller.window {
            controller.isWindowOpen = true
            win.center()
            if let mainWin = self.window {
                if win.parent == nil {
                    mainWin.addChildWindow(win, ordered: .above)
                }
            }
            win.hidesOnDeactivate = true
            win.makeKeyAndOrderFront(nil)
        }
    }

    // MARK: TabManagerDelegate

    func activeTabStateDidChange(title: String, url: String,
                                 canGoBack: Bool, canGoForward: Bool) {
        window?.title = "\(title) — Kitae"
        toolbar.addressField.alwaysShowFullURL = SettingsStore.shared.alwaysShowFullURL
        toolbar.addressField.highlightTopLevelDomain = SettingsStore.shared.highlightTopLevelDomain
        toolbar.addressField.sync(url: url)
        toolbar.addressPill.syncURL(url)
        toolbar.backButton.isEnabled = canGoBack
        toolbar.forwardButton.isEnabled = canGoForward
        // A page that was media and navigated elsewhere loses its bar.
        let activeTabID = tabManager.activeTab?.id
        if let bar = videoPlayerView,
           url != mediaPlayerURL || activeTabID != mediaPlayerTabID {
            bar.detach()
            chrome.showMediaBar(nil)
            chrome.needsLayout = true
            videoPlayerView = nil
            mediaPlayerURL = ""
            mediaPlayerTabID = nil
        }
        if let bar = mediaPlayerView,
           url != mediaPlayerURL || activeTabID != mediaPlayerTabID {
            bar.detach()
            chrome.showMediaBar(nil)
            chrome.needsLayout = true
            mediaPlayerView = nil
            mediaPlayerURL = ""
            mediaPlayerTabID = nil
        }
        reloadTabLists()
    }

    func activeTabLoadingDidChange(_ loading: Bool) {
        chrome.loadingBar.setLoading(loading)
        toolbar.addressPill.setLoading(loading)
    }

    func tabsDidChange() {
        reloadTabLists()
        syncCanvas()
        // H7: keep the saved session in step with the tab set (debounced).
        let snap = tabManager.sessionSnapshot()
        SessionStore.shared.save(urls: snap.urls, activeIndex: snap.activeIndex)
    }

    func tabManagerRequestsCloseTab(at index: Int) {
        guard tabManager.tabs.indices.contains(index) else { return }
        let target = tabManager.tabs[index]
        guard let window else {
            tabManager.closeTab(at: index)
            return
        }
        let alert = NSAlert()
        alert.messageText = "Close the last tab?"
        alert.informativeText = "Kitae will open a new tab after this one closes."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Close Tab")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn, let self,
                  let currentIndex = self.tabManager.tabs.firstIndex(where: { $0 === target })
            else { return }
            self.tabManager.closeTab(at: currentIndex)
        }
    }

    func tabManagerRequestsInsecureNavigation(_ engine: EngineController, url: String) {
        guard tabManager.activeTab?.engine === engine else {
            engine.resolveInsecureNavigation(allow: false)
            return
        }
        guard let window else {
            engine.resolveInsecureNavigation(allow: true)
            return
        }
        let alert = NSAlert()
        alert.messageText = "Open Insecure HTTP Page?"
        alert.informativeText = "\(url) does not use HTTPS. Information sent to this page may be intercepted."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Open")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { response in
            engine.resolveInsecureNavigation(allow: response == .alertFirstButtonReturn)
        }
    }

    func activeTabStatusHint(_ hint: String?) {
        statusHint = hint
        chrome.statusLabel.stringValue = hint ?? ""
        chrome.needsLayout = true
    }

    func activeTabWantsQtsqUnlock() {
        guard let window else { return }
        let alert = NSAlert()
        alert.messageText = "Locked QTSQ file"
        alert.informativeText = "This file is encrypted. Enter its password to open it."
        alert.alertStyle = .informational
        alert.addButton(withTitle: "Unlock")
        alert.addButton(withTitle: "Cancel")
        let input = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        input.placeholderString = "Password"
        alert.accessoryView = input
        alert.window.initialFirstResponder = input
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn,
                  let self, !input.stringValue.isEmpty
            else { return }
            self.tabManager.activeTab?.engine.unlockQtsq(input.stringValue)
        }
    }

    func activeTabSaveDidFinish(path: String, error: String?) {
        let hint: String?
        if let error {
            hint = "QTSQ save failed: \(error)"
        } else if !path.isEmpty {
            hint = "Saved QTSQ archive: \(path)"
        } else {
            hint = nil
        }
        activeTabStatusHint(hint)
    }

    func activeTabWantsFilePick(elementId: String, multiple: Bool) {
        // H2: the page clicked a file input — run the open panel as a sheet
        // and hand the picks to the engine, which mirrors the display into
        // the control and stashes the paths for the multipart submit.
        guard let window else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = multiple
        panel.resolvesAliases = true
        panel.prompt = multiple ? "Choose Files" : "Choose File"
        panel.beginSheetModal(for: window) { [weak self] response in
            guard let self, response == .OK else { return }
            let paths = panel.urls.map(\.path)
            self.tabManager.respondFilePick(elementId: elementId, paths: paths)
        }
    }

    func activeTabWantsAuth(scheme: String, realm: String, url: String) {
        // H6: the page answered 401 with a challenge the engine speaks —
        // collect credentials, optionally remember them, and hand them to
        // the engine (which stores them for the session and re-fetches).
        guard let window else { return }
        let alert = NSAlert()
        alert.messageText = "Sign in"
        alert.informativeText = "\(url)\n\(realm) requires \(scheme) authentication."
        alert.alertStyle = .informational
        alert.addButton(withTitle: "Sign In")
        alert.addButton(withTitle: "Cancel")

        let user = NSTextField(frame: NSRect(x: 0, y: 56, width: 240, height: 24))
        user.placeholderString = "User Name"
        let pass = NSSecureTextField(frame: NSRect(x: 0, y: 28, width: 240, height: 24))
        pass.placeholderString = "Password"
        let remember = NSButton(checkboxWithTitle: "Remember in my Keychain",
                                target: nil, action: nil)
        remember.frame = NSRect(x: 0, y: 0, width: 240, height: 18)
        let stack = NSStackView(views: [user, pass, remember])
        stack.orientation = .vertical
        stack.frame = NSRect(x: 0, y: 0, width: 240, height: 80)
        alert.accessoryView = stack
        alert.window.initialFirstResponder = user

        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn, let self else { return }
            let userName = user.stringValue
            let password = pass.stringValue
            guard !userName.isEmpty else { return }
            if remember.state == .on {
                KeychainStore.save(origin: TabManager.origin(of: url),
                                   user: userName, password: password)
            }
            self.tabManager.respondAuth(user: userName, pass: password)
        }
    }

    func activeTabWantsPasswordSave(origin: String, user: String, password: String) {
        // H6: a submitting form carried a filled password — offer to
        // remember it so the next visit to this origin prefills silently.
        guard let window else { return }
        let alert = NSAlert()
        alert.messageText = "Save password?"
        alert.informativeText = "Keep the sign-in for \(origin) in your Keychain?"
        alert.alertStyle = .informational
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Not Now")
        alert.beginSheetModal(for: window) { response in
            guard response == .alertFirstButtonReturn else { return }
            KeychainStore.save(origin: origin, user: user, password: password)
        }
    }

    // MARK: Inspector (H5)

    func activeTabWantsInspect(node: Int) {
        // H5: a ⌘-click landed on an element — ask the engine for its
        // report; the reply arrives via activeTabInspectInfoDidArrive.
        tabManager.activeTab?.engine.inspectInfo(node: node)
    }

    func activeTabInspectInfoDidArrive(info: String) {
        let panel = inspectorPanel ?? InspectorPanel()
        inspectorPanel = panel
        panel.show(in: window, info: info)
    }

    @objc func toggleInspector(_ sender: Any?) {
        if let panel = inspectorPanel, panel.isVisible { panel.hide(); return }
        activeTabInspectInfoDidArrive(
            info: "⌘-click an element on the page to inspect it.")
    }

    // MARK: Agent panel (B4)

    /// The panel drives the *active tab's* engine: ask, grant, revoke and
    /// read the journal all go through the same verbs kitae_mcp exposes, on
    /// the browser the user is looking at.
    @objc func toggleAgentPanel(_ sender: Any?) {
        if let panel = agentPanel, panel.isVisible { panel.hide(); return }
        let panel = agentPanel ?? AgentPanel()
        agentPanel = panel
        panel.onRequest = { [weak self] request in
            self?.tabManager.activeTab?.engine.agentAct(request)
        }
        panel.onGrant = { [weak self] verb in
            self?.tabManager.activeTab?.engine.agentGrant(verb)
        }
        panel.onRevoke = { [weak self] in
            self?.tabManager.activeTab?.engine.agentRevokeGrants()
        }
        panel.onJournal = { [weak self] in
            self?.tabManager.activeTab?.engine.agentJournal()
        }
        panel.show(in: window)
    }

    func activeTabAgentDidAnswer(reply: String) {
        agentPanel?.apply(reply: reply)
    }

    func activeTabAgentJournalDidArrive(journal: String) {
        agentPanel?.apply(journal: journal)
    }

    func activeTabAgentPolicyDidChange(policy: String) {
        agentPanel?.apply(policy: policy)
    }

    // MARK: Canvas plumbing

    private func reloadTabLists() {
        let showCloseButtons = SettingsStore.shared.showInactiveTabCloseButtons
        let extendPageColor = SettingsStore.shared.extendPageColor
        tabStrip.reload(tabs: tabManager.tabs, activeIndex: tabManager.activeIndex,
                        showCloseButtonsOnInactiveTabs: showCloseButtons,
                        usePageColor: extendPageColor)
        sidebar.reload(tabs: tabManager.tabs, activeIndex: tabManager.activeIndex,
                       showCloseButtonsOnInactiveTabs: showCloseButtons,
                       usePageColor: extendPageColor)
    }

    private func syncCanvas() {
        guard let tab = tabManager.activeTab else { return }
        chrome.showCanvas(tab.page)
        chrome.needsLayout = true
        syncMediaPlayer(tab: tab)
    }

    // MARK: Media transport (J4 audio / J5 video)

    /// The active tab's frame decides which transport bar docks: audio pages
    /// get the AVAudioPlayer bar, video pages the clock-driven video bar,
    /// everything else none.
    private func syncMediaPlayer(tab: Tab) {
        guard let media = tab.latestFrame?.media else {
            detachMediaPlayer()
            return
        }

        switch media.kind {
        case .audio:
            videoPlayerView?.detach()
            if videoPlayerView != nil {
                videoPlayerView = nil
                chrome.showMediaBar(nil)
            }
            syncAudioPlayer(tab: tab, media: media)
        case .video:
            detachAudioPlayer()
            // A `<video controls>` page paints its own transport chrome
            // in-page and the WebVideoDriver advances it — a docked bar
            // would duplicate the same controls. Control-less videos (the
            // QTSQ viewer) keep the docked bar.
            let webControlsVideo = tab.latestFrame?.mediaItems.contains {
                $0.controls && $0.kind == .video
            } ?? false
            if webControlsVideo {
                detachVideoPlayer()
            } else {
                syncVideoPlayer(tab: tab, media: media)
            }
        }
    }

    private func syncAudioPlayer(tab: Tab, media: EngineFrame.MediaInfo) {
        let samePage = mediaPlayerTabID == tab.id
            && mediaPlayerURL == media.url
            && mediaPlayerView != nil
        if !samePage {
            mediaPlayerView?.detach()
            let bar = QTSQAudioPlayerView(frame: NSRect(x: 0, y: 0, width: 100, height: 46))
            bar.onTransport = { [weak self, weak tab] playing, position in
                tab?.engine.mediaPlay(playing: playing, position: position)
                _ = self
            }
            mediaPlayerView = bar
            mediaPlayerURL = media.url
            mediaPlayerTabID = tab.id
            chrome.showMediaBar(bar)
            chrome.needsLayout = true
            // Ask the engine for a fresh publish so the mirror and the bar
            // start in agreement (also re-arms the CAF handoff if the temp
            // file was reaped between frames).
            tab.engine.mediaOpen()
        }
        mediaPlayerView?.update(media: media)
    }

    private func syncVideoPlayer(tab: Tab, media: EngineFrame.MediaInfo) {
        let samePage = mediaPlayerTabID == tab.id
            && mediaPlayerURL == media.url
            && videoPlayerView != nil
        if !samePage {
            videoPlayerView?.detach()
            let bar = QTSQVideoPlayerView(frame: NSRect(x: 0, y: 0, width: 100, height: 46))
            bar.onTransport = { [weak tab] playing, position in
                // Every step goes to the engine: the mirror updates, the
                // published frame re-steps the video's raster, the page
                // shows the frame at that instant.
                tab?.engine.mediaPlay(playing: playing, position: position)
            }
            videoPlayerView = bar
            mediaPlayerURL = media.url
            mediaPlayerTabID = tab.id
            chrome.showMediaBar(bar)
            chrome.needsLayout = true
            // Fresh publish so the mirror and the bar start in agreement.
            tab.engine.mediaOpen()
        }
        videoPlayerView?.update(media: media)
    }

    private func detachMediaPlayer() {
        detachAudioPlayer()
        detachVideoPlayer()
    }

    private func detachAudioPlayer() {
        if let bar = mediaPlayerView {
            bar.detach()
            chrome.showMediaBar(nil)
            chrome.needsLayout = true
        }
        mediaPlayerView = nil
        mediaPlayerURL = ""
        mediaPlayerTabID = nil
    }

    private func detachVideoPlayer() {
        if let bar = videoPlayerView {
            bar.detach()
            chrome.showMediaBar(nil)
            chrome.needsLayout = true
        }
        videoPlayerView = nil
        mediaPlayerURL = ""
        mediaPlayerTabID = nil
    }

    /// The TabManager routes engine callbacks here (it re-exports EngineDelegate).
    func engineDidProduceFrame(_ engine: EngineController, frame: EngineFrame) {
        tabManager.engineDidProduceFrame(engine, frame: frame)
    }

    func engineDidUpdateLoading(_ engine: EngineController, loading: Bool) {
        tabManager.engineDidUpdateLoading(engine, loading: loading)
    }

    func engineWantsExternalOpen(_ engine: EngineController, url: String) {
        tabManager.engineWantsExternalOpen(engine, url: url)
    }

    // MARK: Viewport → engine resize

    func windowDidResize(_ notification: Notification) {
        guard (notification.object as? NSWindow) === window else { return }
        resizeDebounce?.invalidate()
        resizeDebounce = Timer.scheduledTimer(withTimeInterval: 0.12, repeats: false) { [weak self] _ in
            self?.pushResizeToEngines()
        }
    }

    private func pushResizeToEngines() {
        // The floating card clips the canvas; the engine viewport is the
        // canvas' live size so layout matches what's visible. Every tab is
        // resized so background tabs don't come back with a stale viewport.
        let size = chrome.contentCard.bounds.size
        guard size.width > 1, size.height > 1 else { return }
        tabManager.resizeAll(to: size)
    }

    // MARK: Actions

    @objc func newTab(_ sender: Any?) { tabManager.newTab() }
    @objc func closeTab(_ sender: Any?) { tabManager.requestCloseTab(at: tabManager.activeIndex) }
    @objc func reopenClosedTab(_ sender: Any?) { tabManager.reopenLastClosedTab() }
    @objc func reload(_ sender: Any?) { tabManager.reload() }
    @objc func goBack(_ sender: Any?) { tabManager.goBack() }
    @objc func goForward(_ sender: Any?) { tabManager.goForward() }

    @objc func showFullHistory(_ sender: Any?) {
        chrome.closeMenuDrawer()
        tabManager.showFullHistory()
    }

    @objc func clearHistory(_ sender: Any?) {
        guard let window else { return }
        let alert = NSAlert()
        alert.messageText = "Clear Browsing Data"
        alert.informativeText = "Are you sure you want to clear your entire browsing history? This action cannot be undone."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Clear History")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { response in
            if response == .alertFirstButtonReturn {
                HistoryManager.shared.clearAllNow()
            }
        }
    }

    @objc func useTopBar(_ sender: Any?) { setLayoutMode(.topBar) }
    @objc func useSidebar(_ sender: Any?) { setLayoutMode(.sidebar) }
    @objc func toggleSidebarAction(_ sender: Any?) { chrome.toggleSidebar() }

    func setTrafficLightsHidden(_ hidden: Bool) {
        window?.standardWindowButton(.closeButton)?.isHidden = hidden
        window?.standardWindowButton(.miniaturizeButton)?.isHidden = hidden
        window?.standardWindowButton(.zoomButton)?.isHidden = hidden
    }

    private func setLayoutMode(_ mode: ChromeRootView.LayoutMode) {
        SettingsStore.shared.layoutMode = mode
        layoutMode = mode
        chrome.apply(mode: mode, animated: true)
        reloadTabLists()
        syncCanvas()
        pushResizeToEngines()
        settingsController?.refreshFromSettings()
    }

    private func resetSettings() {
        SettingsStore.shared.reset()
        layoutMode = SettingsStore.shared.layoutMode
        Theme.motionReducedByUser = SettingsStore.shared.motionReduced
        applyAppearanceMode()
        chrome.apply(mode: layoutMode, animated: false)
        tabManager.setGPUEnabled(SettingsStore.shared.gpuEnabled)
        tabManager.setSmoothScrolling(SettingsStore.shared.smoothPageScrolling)
        tabManager.setTabCleanupPolicy(SettingsStore.shared.tabCleanupPolicy)
        pushPrivacySettingsToEngines()
        tabManager.setShowDebugBounds(SettingsStore.shared.showDebugBounds)
        toolbar.addressField.alwaysShowFullURL = SettingsStore.shared.alwaysShowFullURL
        toolbar.addressField.highlightTopLevelDomain = SettingsStore.shared.highlightTopLevelDomain
        toolbar.addressField.refreshURL()
        reloadTabLists()
        syncCanvas()
        pushResizeToEngines()
        pushEnvironmentToEngines()
        settingsController?.refreshFromSettings()
    }

    private func copyDiagnosticsReport() {
        let process = ProcessInfo.processInfo
        #if arch(arm64)
        let architecture = "arm64"
        #else
        let architecture = "x86_64"
        #endif
        let active = tabManager.activeTab
        let frame = active?.latestFrame
        let report = """
        Kitae diagnostics
        Generated: \(ISO8601DateFormatter().string(from: Date()))
        App: \(Bundle.main.infoDictionary?["CFBundleName"] as? String ?? "Kitae") \(AppVersion.display)
        Bundle: \(Bundle.main.bundleIdentifier ?? "unknown")
        macOS: \(process.operatingSystemVersionString)
        Architecture: \(architecture)
        Logical CPUs: \(process.activeProcessorCount)
        Memory: \(ByteCountFormatter.string(fromByteCount: Int64(process.physicalMemory), countStyle: .memory))
        Tabs: \(tabManager.tabs.count), active: \(tabManager.activeIndex)
        URL: \(active?.displayedURL ?? "none")
        Loading: \(active?.loading ?? false)
        Canvas: \(frame == nil ? "no active frame" : "engine canvas")
        Frame rects/images/texts: \(frame?.rects.count ?? 0)/\(frame?.images.count ?? 0)/\(frame?.texts.count ?? 0)
        Frame content height: \(frame.map { Int($0.contentHeight) } ?? 0)
        Renderer: GPU \(SettingsStore.shared.gpuEnabled ? "enabled" : "disabled")
        Privacy: DNT \(SettingsStore.shared.sendDNT), block third-party cookies \(SettingsStore.shared.blockThirdPartyCookies), HTTPS-only \(SettingsStore.shared.httpsOnly)
        Search: \(SettingsStore.shared.searchProvider.title)
        """

        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(report, forType: .string)
        let alert = NSAlert()
        alert.messageText = "Diagnostics copied"
        alert.informativeText = "A diagnostics report was copied to the clipboard."
        alert.alertStyle = .informational
        alert.runModal()
    }

    @objc func focusAddressBar(_ sender: Any?) {
        window.makeFirstResponder(toolbar.addressField)
    }

    @objc func showAbout(_ sender: Any?) {
        let alert = NSAlert()
        alert.messageText = "Kitae"
        alert.informativeText = """
        The web, without the noise.

        A JavaScript-free reader browser. Pages arrive as text and links — \
        no scripts run, ever. The engine is pure Rust; this app is its \
        native macOS face.

        \(AppVersion.display)
        """
        alert.alertStyle = .informational
        alert.runModal()
    }
}

// MARK: - Share picker with Export-to-PNG (J6)

/// Bridges the sharing picker's delegate callbacks and owns the "Export to
/// PNG…" custom item: pulls the page's first decoded QTSQ raster (or any
/// frame image) through the engine cache and saves it via NSSavePanel.
final class SharePickerDelegate: NSObject, NSSharingServicePickerDelegate {

    private weak var page: Tab?

    init(page: Tab) {
        self.page = page
    }

    func sharingServicePicker(_ sharingServicePicker: NSSharingServicePicker,
                              sharingServicesForItems items: [Any],
                              proposedSharingServices proposedServices: [NSSharingService])
        -> [NSSharingService] {
        var services = proposedServices
        let export = NSSharingService(
            title: "Export to PNG…",
            image: NSImage(systemSymbolName: "square.and.arrow.down",
                           accessibilityDescription: "Export to PNG") ?? NSImage(),
            alternateImage: nil,
            handler: { [weak self] in self?.exportPageToPNG() })
        services.insert(export, at: 0)
        // Save-Page-as-QTSQ: the page itself becomes a device-encrypted
        // archive; the engine reports the outcome through engineDidFinishSave.
        let saveQtsq = NSSharingService(
            title: "Save as QTSQ…",
            image: NSImage(systemSymbolName: "doc.badge.lock",
                           accessibilityDescription: "Save as QTSQ") ?? NSImage(),
            alternateImage: nil,
            handler: { [weak self] in self?.savePageAsQtsq() })
        services.insert(saveQtsq, at: 1)
        return services
    }

    private func savePageAsQtsq() {
        guard let page else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [UTType(filenameExtension: "qtsq") ?? .data]
        panel.nameFieldStringValue = suggestedQtsqName(for: page)
        guard panel.runModal() == .OK, let url = panel.url else { return }
        page.engine.savePageQtsq(to: url.path)
    }

    private func suggestedQtsqName(for tab: Tab) -> String {
        let base = (tab.displayedURL as NSString).lastPathComponent
        let stem = base.hasSuffix(".qtsq") ? String(base.dropLast(5)) : base
        return (stem.isEmpty ? "page" : stem) + ".qtsq"
    }

    private func exportPageToPNG() {
        guard let frame = page?.latestFrame,
              let image = frame.images.first(where: { $0.payload != 0 })
        else {
            exportPlaceholderAlert()
            return
        }
        guard let cg = PageCanvasRaster.fetch(image.payload) else {
            exportPlaceholderAlert()
            return
        }
        savePNG(cg, suggestedName: suggestedName(for: frame))
    }

    private func suggestedName(for frame: EngineFrame) -> String {
        let base = (frame.url as NSString).lastPathComponent
        let stem = base.hasSuffix(".qtsq") ? String(base.dropLast(5)) : base
        return (stem.isEmpty ? "image" : stem) + ".png"
    }

    private func exportPlaceholderAlert() {
        let alert = NSAlert()
        alert.messageText = "Nothing to export yet"
        alert.informativeText = "This page has no decoded image pixels."
        alert.alertStyle = .informational
        alert.runModal()
    }

    private func savePNG(_ cg: CGImage, suggestedName: String) {
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.png]
        panel.nameFieldStringValue = suggestedName
        guard panel.runModal() == .OK, let url = panel.url else { return }

        let rep = NSBitmapImageRep(cgImage: cg)
        guard let data = rep.representation(using: .png, properties: [:]) else { return }
        try? data.write(to: url)
    }
}

/// Thin wrapper so the share sheet can reach PageCanvasView's raster
/// decoder without making it public API.
enum PageCanvasRaster {
    static func fetch(_ payload: UInt64) -> CGImage? {
        let total = kitae_qtsq_raster_size(payload)
        guard total >= 8 else { return nil }
        var buf = [UInt8](repeating: 0, count: total)
        guard kitae_qtsq_raster(payload, &buf, buf.count) == 1 else { return nil }
        let w = Int(buf[0]) | Int(buf[1]) << 8 | Int(buf[2]) << 16 | Int(buf[3]) << 24
        let h = Int(buf[4]) | Int(buf[5]) << 8 | Int(buf[6]) << 16 | Int(buf[7]) << 24
        guard w > 0, h > 0, buf.count >= 8 + w * h * 4 else { return nil }
        var px = buf
        for i in stride(from: 8, to: 8 + w * h * 4, by: 4) {
            let a = px[i + 3]
            if a != 255 {
                px[i] = UInt8((Int(px[i]) * Int(a) + 127) / 255)
                px[i + 1] = UInt8((Int(px[i + 1]) * Int(a) + 127) / 255)
                px[i + 2] = UInt8((Int(px[i + 2]) * Int(a) + 127) / 255)
            }
        }
        let ctx = CGContext(data: &px, width: w, height: h,
                            bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpaceCreateDeviceRGB(),
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        return ctx?.makeImage()
    }
}
