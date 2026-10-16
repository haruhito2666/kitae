// TabManager.swift — multi-tab ownership and routing.
//
// One tab == one engine instance (one OS thread) + one canvas view. The
// manager polls every engine on a shared display-rate timer, routes frames
// to canvases, forwards chrome state for the active tab, and dispatches
// navigation commands.

import AppKit

final class Tab {
    let id = UUID()
    let engine: EngineController
    let canvas: PageCanvasView
    /// GPU layer stack (Phase L5): Metal bitmap behind, text canvas front.
    /// `page` is what the chrome shows; `canvas` keeps events/scrollbar.
    let metalView: MetalCanvasView
    let page: NSView
    /// Present GPU tiles when the Settings toggle is on.
    var gpuOn = false
    private(set) var latestFrame: EngineFrame?
    private(set) var loading = false
    private(set) var lastUsedAt = Date()

    func markUsed() {
        lastUsedAt = Date()
    }

    /// Address currently shown in the field for this tab (synced from frames).
    private(set) var displayedURL: String = ""

    init(viewport: CGSize, delegate: EngineDelegate) {
        canvas = PageCanvasView(frame: NSRect(x: 0, y: 0, width: max(1, viewport.width),
                                              height: max(1, viewport.height)))
        metalView = MetalCanvasView(frame: canvas.frame)
        page = NSView(frame: canvas.frame)
        metalView.frame = page.bounds
        metalView.autoresizingMask = [.width, .height]
        canvas.autoresizingMask = [.width, .height]
        page.addSubview(metalView)
        page.addSubview(canvas)
        metalView.isHidden = true
        engine = EngineController(viewport: viewport, delegate: delegate)
    }

    private(set) var isShowingHistory = false
    private var historyView: HistoryPageView?
    /// H3: true while a print run holds the engine (print media + the paper
    /// viewport). The print frame goes to `PagePrinter`, not to the screen, so
    /// frames are neither adopted nor painted until the run restores screen
    /// media and the canvas repaints from the next frame.
    private(set) var isPrinting = false

    /// Suppress on-screen adoption for the duration of a print run. Returns
    /// true when this call turned the gate on, so the caller knows it owns
    /// the matching `endPrintRun()`.
    @discardableResult
    func beginPrintRun() -> Bool {
        let wasPrinting = isPrinting
        isPrinting = true
        return !wasPrinting
    }

    /// Release the gate. The engine has already been put back on screen media
    /// by then, so the next published frame repaints the canvas.
    func endPrintRun() {
        isPrinting = false
    }

    func adoptFrame(_ frame: EngineFrame) {
        // A print-media frame is for the printer; the screen keeps whatever it
        // was showing until screen media is restored.
        guard !isPrinting else { return }
        latestFrame = frame
        if !isShowingHistory {
            displayedURL = frame.url
            if !frame.url.isEmpty && frame.url != "about:blank" && frame.url != "about:start" {
                HistoryManager.shared.add(title: frame.title, url: frame.url)
            }
            presentGPU()
            canvas.applyFrame(frame)
        }
    }

    func displayHistoryPage(onNavigate: @escaping (String) -> Void, onOpenNewTab: @escaping (String) -> Void) {
        isShowingHistory = true
        displayedURL = "about:history"
        metalView.isHidden = true
        canvas.isHidden = true
        if historyView == nil {
            let hv = HistoryPageView(frame: page.bounds)
            hv.autoresizingMask = [.width, .height]
            page.addSubview(hv)
            historyView = hv
        }
        historyView?.frame = page.bounds
        historyView?.isHidden = false
        historyView?.onNavigate = { [weak self] url in
            self?.dismissHistoryPage()
            onNavigate(url)
        }
        historyView?.onOpenNewTab = { url in
            onOpenNewTab(url)
        }
    }

    func dismissHistoryPage() {
        guard isShowingHistory else { return }
        isShowingHistory = false
        historyView?.removeFromSuperview()
        historyView = nil
        canvas.isHidden = false
        if let frame = latestFrame {
            displayedURL = frame.url
            presentGPU()
        } else {
            displayedURL = ""
        }
    }

    /// Render the latest frame's layers to the Metal view (no-op unless the
    /// toggle is on and presentation succeeds — then the canvas drops to
    /// text-only overlay mode).
    func presentGPU() {
        guard gpuOn, MetalCanvasView.isAvailable, let frame = latestFrame,
              frame.scrollContainers.isEmpty else {
            metalView.isHidden = true
            canvas.textOnly = false
            return
        }
        let raw = engine.lastFrameBytes
        let size = canvas.bounds.size
        let scale = Float(canvas.window?.backingScaleFactor ?? 2.0)
        let w = max(1, Int(size.width * CGFloat(scale)))
        let h = max(1, Int(size.height * CGFloat(scale)))
        guard !raw.isEmpty,
              let out = engine.renderGPUTile(width: w, height: h, scale: scale,
                                             scroll: CGPoint(x: 0, y: canvas.scrollOffset)),
              metalView.present(pixels: out.pixels, width: out.w, height: out.h)
        else {
            metalView.isHidden = true
            canvas.textOnly = false
            return
        }
        metalView.isHidden = false
        canvas.textOnly = true
        canvas.needsDisplay = true
    }

    func setLoading(_ loading: Bool) {
        self.loading = loading
    }
}

protocol TabManagerDelegate: AnyObject {
    /// Chrome state of the active tab changed (title, URL, back/forward).
    func activeTabStateDidChange(title: String, url: String,
                                 canGoBack: Bool, canGoForward: Bool)
    /// Loading state of the active tab changed.
    func activeTabLoadingDidChange(_ loading: Bool)
    /// The set of tabs changed structurally (open/close/switch).
    func tabsDidChange()
    func tabManagerRequestsCloseTab(at index: Int)
    func tabManagerRequestsInsecureNavigation(_ engine: EngineController, url: String)
    /// A status-hint (hovered link) for the active tab.
    func activeTabStatusHint(_ hint: String?)
    /// The active tab's page is a password-locked QTSQ; the host offers the
    /// unlock prompt and calls back through `EngineController.unlockQtsq`.
    func activeTabWantsQtsqUnlock()
    /// H2: the active tab's page clicked `input[type=file]`; the host offers
    /// the open panel and answers through `TabManager.respondFilePick`.
    func activeTabWantsFilePick(elementId: String, multiple: Bool)
    /// H6: the active tab's page answered 401 with a Basic/Digest
    /// challenge; the host offers the sign-in sheet and answers through
    /// `TabManager.respondAuth`.
    func activeTabWantsAuth(scheme: String, realm: String, url: String)
    /// H6: a filled password input was in the active tab's submitting
    /// form; the host offers to save it in the Keychain.
    func activeTabWantsPasswordSave(origin: String, user: String, password: String)
    /// H5: the active tab's page was ⌘-clicked for the inspector; the host
    /// asks that engine for the element's report via `inspectInfo`.
    func activeTabWantsInspect(node: Int)
    /// H5: the active tab's picked element's report arrived; the host
    /// shows it in its inspector panel.
    /// B4: one agent verb was answered on the active tab's engine (the reply
    /// JSON travels with it, journal rows included).
    func activeTabAgentDidAnswer(reply: String)
    /// B4: the active tab's whole activity journal arrived (JSON array).
    func activeTabAgentJournalDidArrive(journal: String)
    /// B4: the active tab's grants/declarations changed (JSON object).
    func activeTabAgentPolicyDidChange(policy: String)
    func activeTabInspectInfoDidArrive(info: String)
    /// A Save-Page-as-QTSQ request finished on the active tab: `error` is
    /// nil and `path` names the archive written, or non-nil with the reason.
    func activeTabSaveDidFinish(path: String, error: String?)
}

final class TabManager: NSObject, EngineDelegate {

    private(set) var tabs: [Tab] = []
    private(set) var activeIndex = 0
    private var lastLoadingState = false
    /// Locked-QTSQ URL we last prompted for, per tab: a cancelled prompt or
    /// a wrong password must not re-prompt the same page until it navigates
    /// elsewhere (frames keep carrying the flag on every republish).
    private var qtsqPromptedURLs: [UUID: String] = [:]

    /// H7: recently closed tabs (most recent first, capped) for
    /// Reopen Closed Tab. Start-like pages never enter the stack.
    private(set) var closedTabURLs: [String] = []
    private let closedTabURLLimit = 20

    weak var delegate: TabManagerDelegate?

    /// Poll timer shared by all engines (main run loop, common modes so it
    /// keeps ticking while menus track or the user scrolls).
    private var pollTimer: Timer?
    /// P-2 (SECURITY_AUDIT.md): the poll rate adapts to activity. Fast while
    /// any engine is busy (loading or publishing frames), then backs off to
    /// an idle rate after ~1 s of quiescence. The consume-on-read frame
    /// protocol makes the slow path lossless: frames simply wait in the
    /// single-slot mailbox until the next tick.
    private let pollFastInterval = 1.0 / 60.0
    private let pollIdleInterval = 1.0 / 10.0
    /// Fast ticks with no activity before dropping to the idle rate (~1 s).
    private let pollIdleAfterTicks = 60
    private var idleTicks = 0
    private var cleanupTimer: Timer?
    private var cleanupPolicy: TabCleanupPolicy = SettingsStore.shared.tabCleanupPolicy
    private let cleanupInterval: TimeInterval = 5 * 60

    var activeTab: Tab? {
        tabs.indices.contains(activeIndex) ? tabs[activeIndex] : nil
    }

    func start() {
        schedulePoll(interval: pollFastInterval)
        cleanupTimer?.invalidate()
        let timer = Timer(timeInterval: cleanupInterval, repeats: true) { [weak self] _ in
            self?.performInactiveTabCleanup()
        }
        RunLoop.main.add(timer, forMode: .common)
        cleanupTimer = timer
        performInactiveTabCleanup()
    }

    func stop() {
        pollTimer?.invalidate()
        pollTimer = nil
        cleanupTimer?.invalidate()
        cleanupTimer = nil
        idleTicks = 0
    }

    /// One self-rescheduling tick: poll every tab, then schedule the next
    /// tick at the rate the activity level deserves.
    private func pollTick() {
        let busy = pollAll()
        if busy {
            idleTicks = 0
            schedulePoll(interval: pollFastInterval)
        } else {
            idleTicks += 1
            let interval = idleTicks >= pollIdleAfterTicks
                ? pollIdleInterval
                : pollFastInterval
            schedulePoll(interval: interval)
        }
    }

    private func schedulePoll(interval: Double) {
        let timer = Timer(timeInterval: interval, repeats: false) { [weak self] _ in
            self?.pollTick()
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    // MARK: Tab lifecycle

    func newTab() {
        let viewport = activeTab?.canvas.bounds.size ?? CGSize(width: 900, height: 600)
        let tab = Tab(viewport: viewport, delegate: self)
        tab.gpuOn = RenderSettings.gpuOn
        tab.canvas.setSmoothScrolling(SettingsStore.shared.smoothPageScrolling)
        tab.canvas.showDebugBounds = SettingsStore.shared.showDebugBounds
        tab.canvas.onLinkClick = { [weak self] href in
            self?.navigateActive(href)
        }
        tab.canvas.onScrollChange = { [weak self, weak tab] in
            tab?.presentGPU()
            if let self, let tab, self.activeTab === tab {
                tab.markUsed()
            }
        }
        tab.canvas.onStatusChange = { [weak self, weak tab] hint in
            guard let self, let tab, self.activeTab === tab else { return }
            self.delegate?.activeTabStatusHint(hint)
        }
        // A11a: non-link clicks and editing keys drive the engine's caret.
        tab.canvas.onClick = { [weak tab] x, y in
            tab?.engine.click(x: Float(x), y: Float(y))
        }
        // H5: ⌘-click picks the element under the point for the inspector;
        // the pick notification routes back through the delegate below.
        tab.canvas.onInspectClick = { [weak tab] x, y in
            tab?.engine.inspectPick(x: Float(x), y: Float(y))
        }
        tab.canvas.onInsertText = { [weak tab] text in
            tab?.engine.typeText(text)
        }
        tab.canvas.onMarkedText = { [weak tab] text in
            tab?.engine.updateMarkedText(text)
        }
        tab.canvas.onCompositionCommit = { [weak tab] text in
            if let text {
                tab?.engine.commitComposition(text)
            } else {
                tab?.engine.cancelComposition()
            }
        }
        tab.canvas.onCompositionCancel = { [weak tab] in
            tab?.engine.cancelComposition()
        }
        tab.canvas.onStepFocus = { [weak tab] direction in
            tab?.engine.stepFocus(direction)
        }
        tab.canvas.onSelectionAction = { [weak tab] action in
            guard let tab else { return }
            switch action {
            case .begin(let offset): tab.engine.beginSelection(offset)
            case .extend(let offset): tab.engine.extendSelection(offset)
            case .beginPoint(let x, let y): tab.engine.beginSelection(x: Float(x), y: Float(y))
            case .extendPoint(let x, let y): tab.engine.extendSelection(x: Float(x), y: Float(y))
            case .end: tab.engine.endSelection()
            }
        }
        tab.canvas.onKeyAction = { [weak tab] action in
            guard let tab else { return }
            switch action {
            case .backspace: tab.engine.backspace()
            case .deleteForward: tab.engine.deleteForward()
            case .caretLeft: tab.engine.moveCaret(-1)
            case .caretRight: tab.engine.moveCaret(1)
            case .caretHome: tab.engine.setCaret(0)
            // End: a large forward step clamps to the value's length.
            case .caretEnd: tab.engine.moveCaret(1_000_000)
            case .extendCaretLeft: tab.engine.extendCaret(-1)
            case .extendCaretRight: tab.engine.extendCaret(1)
            case .extendCaretHome: tab.engine.extendCaret(-1_000_000)
            case .extendCaretEnd: tab.engine.extendCaret(1_000_000)
            // H4: roving focus — Tab/⇧Tab cycle the engine's focusables.
            case .focusNext: tab.engine.focusNext()
            case .focusPrev: tab.engine.focusPrev()
            }
        }
        tabs.append(tab)
        activate(index: tabs.count - 1)
    }

    func requestCloseTab(at index: Int) {
        guard tabs.indices.contains(index) else { return }
        if tabs.count == 1, SettingsStore.shared.warnBeforeClosingLastTab, let delegate {
            delegate.tabManagerRequestsCloseTab(at: index)
        } else {
            closeTab(at: index)
        }
    }

    func closeTab(at index: Int) {
        guard tabs.indices.contains(index) else { return }
        let wasActive = index == activeIndex
        let closing = tabs[index]
        WebVideoDriver.shared.stop(tab: closing)
        // H7: real pages land on the reopen stack; start-like pages don't.
        if !isTransientURL(closing.displayedURL) {
            closedTabURLs.insert(closing.displayedURL, at: 0)
            if closedTabURLs.count > closedTabURLLimit {
                closedTabURLs.removeLast(closedTabURLs.count - closedTabURLLimit)
            }
        }
        tabs.remove(at: index)
        qtsqPromptedURLs.removeValue(forKey: closing.id)
        if tabs.isEmpty {
            newTab()  // never leave the window without a tab
            return
        }
        if wasActive {
            activeIndex = min(index, tabs.count - 1)
            emitActiveState()
        } else if index < activeIndex {
            activeIndex -= 1
        }
        delegate?.tabsDidChange()
        emitActiveState()
    }

    func activate(index: Int) {
        guard tabs.indices.contains(index) else { return }
        let previous = activeTab
        activeIndex = index
        tabs[index].markUsed()
        // A tab created or last laid out at a different window size gets the
        // current viewport on activation so its next frame fits the card.
        if let tab = activeTab, let previous, tab !== previous {
            tab.engine.resize(to: previous.canvas.bounds.size)
        }
        delegate?.tabsDidChange()
        emitActiveState()
        // A background tab that loaded a locked QTSQ prompts on activation.
        if let tab = activeTab, let frame = tab.latestFrame, frame.qtsqLocked {
            maybePromptQtsqUnlock(tab, url: frame.url)
        }
    }

    // MARK: Session restore & closed tabs (H7)

    /// URLs that are not real session state (start/history/blank pages).
    private func isTransientURL(_ url: String) -> Bool {
        url.isEmpty || url == "about:blank" || url == "about:start" || url == "about:history"
    }

    /// H7: pop the most recently closed tab's URL into a new tab. Returns
    /// whether there was anything to reopen.
    @discardableResult
    func reopenLastClosedTab() -> Bool {
        guard let url = closedTabURLs.first else { return false }
        closedTabURLs.removeFirst()
        newTab()
        navigateActive(url)
        return true
    }

    /// H7: the persisted session — non-transient tab URLs and the active
    /// index among them (an empty URL list means only start pages are open;
    /// the store then clears the saved session).
    func sessionSnapshot() -> (urls: [String], activeIndex: Int) {
        var urls: [String] = []
        var active = 0
        for (i, tab) in tabs.enumerated() {
            if isTransientURL(tab.displayedURL) { continue }
            if i == activeIndex { active = urls.count }
            urls.append(tab.displayedURL)
        }
        return (urls, active)
    }

    /// H7: restore the saved session (one tab per URL, the saved tab
    /// active). Returns false when there is nothing to restore — first
    /// launch, or the last session ended with only start pages open.
    @discardableResult
    func restoreSession() -> Bool {
        guard let state = SessionStore.shared.load() else { return false }
        for url in state.urls {
            newTab()
            navigateActive(url)
        }
        if tabs.count > 1 {
            activate(index: min(max(0, state.activeIndex), tabs.count - 1))
        }
        return true
    }

    /// Offer the unlock prompt once per page visit per tab; the delegate
    /// answers through `EngineController.unlockQtsq`.
    private func maybePromptQtsqUnlock(_ tab: Tab, url: String) {
        guard qtsqPromptedURLs[tab.id] != url else { return }
        qtsqPromptedURLs[tab.id] = url
        delegate?.activeTabWantsQtsqUnlock()
    }

    // MARK: Commands (routed to the active tab)

    func showFullHistory() {
        activeTab?.markUsed()
        activeTab?.displayHistoryPage(onNavigate: { [weak self] url in
            self?.navigateActive(url)
        }, onOpenNewTab: { [weak self] url in
            self?.newTab()
            self?.navigateActive(url)
        })
        emitActiveState()
        delegate?.tabsDidChange()
    }

    func navigateActive(_ input: String) {
        let clean = input.trimmingCharacters(in: .whitespaces)
        guard !clean.isEmpty else { return }
        if clean.lowercased() == "about:history" || clean.lowercased() == "kitae://history" || clean.lowercased() == "history" {
            showFullHistory()
            return
        }
        activeTab?.markUsed()
        activeTab?.dismissHistoryPage()
        activeTab?.engine.navigate(clean)
    }

    func goBack() {
        activeTab?.markUsed()
        if activeTab?.isShowingHistory == true {
            activeTab?.dismissHistoryPage()
            emitActiveState()
            delegate?.tabsDidChange()
            return
        }
        activeTab?.engine.goBack()
    }
    func goForward() {
        activeTab?.markUsed()
        activeTab?.engine.goForward()
    }
    func reload() {
        activeTab?.markUsed()
        if activeTab?.isShowingHistory == true {
            showFullHistory()
            return
        }
        activeTab?.engine.reload()
    }

    /// Push a new viewport to every engine (window resized / layout changed).
    func resizeAll(to size: CGSize) {
        for tab in tabs {
            tab.engine.resize(to: size)
        }
    }

    func setSmoothScrolling(_ enabled: Bool) {
        for tab in tabs {
            tab.canvas.setSmoothScrolling(enabled)
        }
    }

    func setShowDebugBounds(_ enabled: Bool) {
        for tab in tabs {
            tab.canvas.showDebugBounds = enabled
        }
    }

    func setTabCleanupPolicy(_ policy: TabCleanupPolicy) {
        cleanupPolicy = policy
        performInactiveTabCleanup()
    }

    func performInactiveTabCleanup() {
        guard let interval = cleanupPolicy.interval else { return }
        let cutoff = Date().addingTimeInterval(-interval)
        let candidates = tabs.indices.filter { index in
            index != activeIndex
                && !tabs[index].isShowingHistory
                && !tabs[index].loading
                && !tabs[index].engine.isLoading
                && tabs[index].lastUsedAt <= cutoff
        }
        for index in candidates.reversed() {
            closeTab(at: index)
        }
    }

    func clearPageCaches() {
        kitae_purge_image_caches()
        WebVideoDriver.shared.clearAll()
        for tab in tabs {
            tab.engine.clearPageCaches()
            tab.canvas.clearPageCaches()
            tab.metalView.clearCache()
            if !tab.isShowingHistory, tab.latestFrame != nil {
                tab.engine.reload()
            }
        }
    }

    // MARK: Polling

    /// Flip the GPU tile path for every tab (Settings toggle); the active
    /// tab re-presents its latest frame immediately.
    func setGPUEnabled(_ on: Bool) {
        for tab in tabs {
            tab.gpuOn = on
        }
        activeTab?.presentGPU()
    }

    func setPrivacyPolicy(dnt: Bool, blockThirdPartyCookies: Bool, httpsOnly: Bool,
                          warnInsecureHTTP: Bool) {
        for tab in tabs {
            tab.engine.setPrivacyPolicy(
                dnt: dnt,
                blockThirdPartyCookies: blockThirdPartyCookies,
                httpsOnly: httpsOnly,
                warnInsecureHTTP: warnInsecureHTTP
            )
        }
    }

    func setSearchProvider(_ provider: SearchProviderSetting) {
        for tab in tabs {
            tab.engine.setSearchProvider(provider)
        }
    }

    /// Push the environment (color scheme + reduce motion) to every engine
    /// (Settings toggles); pages styled with `@media (prefers-…)` re-render.
    func setEnvironment(dark: Bool, reducedMotion: Bool) {
        for tab in tabs {
            tab.engine.setEnvironment(dark: dark, reducedMotion: reducedMotion)
        }
    }

    /// Poll every tab, returning whether anything made work happen (a frame
    /// was produced or a navigation is in flight) — the P-2 activity signal.
    private func pollAll() -> Bool {
        var busy = false
        for tab in tabs {
            if tab.engine.poll() != nil || tab.engine.isLoading {
                busy = true
            }
        }
        return busy
    }

    // MARK: EngineDelegate

    func engineDidProduceFrame(_ engine: EngineController, frame: EngineFrame) {
        guard let index = tabs.firstIndex(where: { $0.engine === engine }) else { return }
        let tab = tabs[index]
        if index == activeIndex {
            tab.markUsed()
        }
        tab.adoptFrame(frame)
        WebVideoDriver.shared.observe(tab: tab, frame: frame)
        // Unlocked (or never locked): forget any prompt we owed this page.
        if !frame.qtsqLocked {
            qtsqPromptedURLs.removeValue(forKey: tabs[index].id)
        }
        // Locked QTSQ on the visible tab: offer the unlock prompt.
        if frame.qtsqLocked, index == activeIndex {
            maybePromptQtsqUnlock(tabs[index], url: frame.url)
        }
        if index == activeIndex {
            emitActiveState()
            delegate?.tabsDidChange() // canvas swap + URL sync
        }
    }

    func engineDidUpdateLoading(_ engine: EngineController, loading: Bool) {
        guard let index = tabs.firstIndex(where: { $0.engine === engine }) else { return }
        tabs[index].setLoading(loading)
        if index == activeIndex {
            lastLoadingState = loading
            delegate?.activeTabLoadingDidChange(loading)
        }
    }

    func engineWantsExternalOpen(_ engine: EngineController, url: String) {
        // mailto: and kitae:open-external: targets — the OS's job, never ours.
        // Defense in depth for S-2 (SECURITY_AUDIT.md): the engine already
        // only queues http/https/mailto targets; mirror that allowlist here
        // so no engine path can ever hand an arbitrary scheme (file:,
        // installed-app schemes, …) to the OS.
        guard let target = URL(string: url),
              let scheme = target.scheme?.lowercased(),
              ["https", "http", "mailto"].contains(scheme) else { return }
        NSWorkspace.shared.open(target)
    }

    func engineDidFinishSave(_ engine: EngineController, path: String, error: String?) {
        // Saves are triggered from the active tab's share picker, so only
        // that tab's outcome reaches the chrome.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabSaveDidFinish(path: path, error: error)
    }

    func engineDidCrash(_ engine: EngineController, reason: String) {
        // H8: the engine thread recovered — the tab now paints the engine's
        // crash page (its Reload link refetches) and keeps serving commands.
        // Surface the event in the status bar; background tabs stay quiet
        // until activated.
        guard let index = tabs.firstIndex(where: { $0.engine === engine }) else { return }
        if index == activeIndex {
            delegate?.activeTabStatusHint(
                "This tab crashed — Kitae recovered it. Use the page's Reload link or ⌘R. (\(reason))")
        }
    }

    func engineDidNotifyDownload(_ engine: EngineController, item: DownloadItem) {
        // H1: every tab's download narration merges into the shared ledger
        // (the popover and toolbar badge observe DownloadCenter directly).
        // Only terminal states surface a toast — active progress would spam
        // the status bar at the coalescing cadence.
        let previous = DownloadCenter.shared.items.first(where: { $0.id == item.id })?.state
        DownloadCenter.shared.apply(item)
        guard item.state != previous else { return }
        if item.isFailed {
            delegate?.activeTabStatusHint(
                "Download failed: \(item.failureMessage ?? "unknown error")")
        } else if item.isFinished {
            delegate?.activeTabStatusHint("Downloaded \(item.filename)")
        }
    }

    func engineDidRequestFilePick(_ engine: EngineController, elementId: String, multiple: Bool) {
        // H2: only the active tab may summon the open panel — a background
        // tab's request would otherwise yank focus mid-typing.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabWantsFilePick(elementId: elementId, multiple: multiple)
    }

    func engineDidRequestAuth(_ engine: EngineController, scheme: String, realm: String, url: String) {
        // H6: only the active tab may summon the sign-in sheet.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabWantsAuth(scheme: scheme, realm: realm, url: url)
    }

    func engineDidFindLogin(_ engine: EngineController, formId: String, userFieldId: String,
                            passFieldId: String) {
        // H6: prefill silently from the Keychain — no prompt, any tab. The
        // tab's frame URL keys the lookup, mirroring the origin the engine
        // reports on `pwsave` for the same page.
        guard let tab = tabs.first(where: { $0.engine === engine }),
              let frameUrl = tab.latestFrame?.url,
              let saved = KeychainStore.lookup(origin: Self.origin(of: frameUrl))
        else { return }
        engine.setInput(id: userFieldId, value: saved.user)
        engine.setInput(id: passFieldId, value: saved.password)
    }

    func engineDidSubmitLoginForm(_ engine: EngineController, origin: String, user: String,
                                  password: String) {
        // H6: only the active tab may raise the save prompt.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabWantsPasswordSave(origin: origin, user: user, password: password)
    }

    func engineDidPickInspect(_ engine: EngineController, node: Int) {
        // H5: only the visible tab drives the inspector panel — a background
        // tab's ⌘-click would otherwise hijack the readout.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabWantsInspect(node: node)
    }

    func engineDidReceiveInspectInfo(_ engine: EngineController, info: String) {
        // H5: show only the active tab's report (the pick that asked for it).
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabInspectInfoDidArrive(info: info)
    }

    func engineDidAnswerAgent(_ engine: EngineController, reply: String) {
        // B4: only the active tab's agent is on screen — a background tab's
        // engine can be driven by nothing today, and an unattended prompt
        // would be a button the user cannot see.
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabAgentDidAnswer(reply: reply)
    }

    func engineDidReceiveAgentJournal(_ engine: EngineController, journal: String) {
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabAgentJournalDidArrive(journal: journal)
    }

    func engineDidUpdateAgentPolicy(_ engine: EngineController, policy: String) {
        guard tabs.firstIndex(where: { $0.engine === engine }) == activeIndex else { return }
        delegate?.activeTabAgentPolicyDidChange(policy: policy)
    }

    func engineDidRequestInsecureNavigation(_ engine: EngineController, url: String) {
        delegate?.tabManagerRequestsInsecureNavigation(engine, url: url)
    }

    /// H6: the sign-in sheet's answer, sent to the active tab's engine —
    /// the engine stores the session credentials and re-fetches the URL.
    func respondAuth(user: String, pass: String) {
        activeTab?.engine.setAuth(user: user, pass: pass)
    }

    /// H6: scheme://authority of `url`, cut at the first /, ? or # — the
    /// same slice the engine's `origin_of` produces for its pwsave key.
    static func origin(of url: String) -> String {
        guard let sep = url.range(of: "://") else { return url }
        let rest = url[sep.upperBound...]
        let end = rest.firstIndex(where: { $0 == "/" || $0 == "?" || $0 == "#" }) ?? rest.endIndex
        return String(url[..<end])
    }

    /// H2: the open-panel answer, sent to the active tab's engine. An empty
    /// `paths` clears the pick; the engine mirrors the display into the
    /// control's value attribute itself.
    func respondFilePick(elementId: String, paths: [String]) {
        activeTab?.engine.setFileFiles(id: elementId, paths: paths)
    }

    // MARK: Chrome state

    private func emitActiveState() {
        guard let tab = activeTab else {
            delegate?.activeTabStateDidChange(title: "New Tab", url: "",
                                              canGoBack: false, canGoForward: false)
            delegate?.activeTabLoadingDidChange(false)
            return
        }
        if tab.isShowingHistory {
            delegate?.activeTabStateDidChange(title: "History",
                                              url: "about:history",
                                              canGoBack: tab.latestFrame != nil,
                                              canGoForward: false)
            delegate?.activeTabLoadingDidChange(false)
            return
        }
        guard let frame = tab.latestFrame else {
            delegate?.activeTabStateDidChange(title: "New Tab", url: "",
                                              canGoBack: false, canGoForward: false)
            delegate?.activeTabLoadingDidChange(false)
            return
        }
        delegate?.activeTabStateDidChange(title: frame.title.isEmpty ? "New Tab" : frame.title,
                                          url: frame.url,
                                          canGoBack: frame.canGoBack,
                                          canGoForward: frame.canGoForward)
        delegate?.activeTabLoadingDidChange(tab.loading)
    }
}
