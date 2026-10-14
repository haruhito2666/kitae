// PageCanvasView.swift — paints engine frames with Core Text.
//
// The canvas owns view-local scrolling (the engine's scroll is informational;
// frames always carry the whole document). It draws rects first, then text
// runs; link hit-areas are kept for click translation and cursor feedback.
// The overlay scrollbar lives here too.

import AppKit

/// A11a: editing keys the canvas forwards to the engine's caret model. The
/// engine owns the caret; the view only reports what the user pressed.
/// H4 adds the roving-focus pair: Tab/⇧Tab cycle the engine's focusables.
enum CanvasKeyAction {
    case backspace, deleteForward
    case caretLeft, caretRight
    case caretHome, caretEnd
    case extendCaretLeft, extendCaretRight
    case extendCaretHome, extendCaretEnd
    case focusNext, focusPrev
}

enum CanvasSelectionAction {
    case begin(Int)
    case extend(Int)
    case beginPoint(CGFloat, CGFloat)
    case extendPoint(CGFloat, CGFloat)
    case end
}

final class PageCanvasView: NSView, NSTextInputClient {

    // MARK: State

    private(set) var pageFrame: EngineFrame?
    /// View-local scroll offset in points (0 = document top).
    private(set) var scrollOffset: CGFloat = 0
    var smoothScrolling = SettingsStore.shared.smoothPageScrolling
    var showDebugBounds = false {
        didSet {
            if showDebugBounds != oldValue { needsDisplay = true }
        }
    }
    private var scrollTimer: Timer?
    private var scrollStart: CGFloat = 0
    private var scrollTarget: CGFloat = 0
    private var containerOffsets: [UInt32: CGFloat] = [:]
    private var containerXOffsets: [UInt32: CGFloat] = [:]
    private var scrollbarDrag: (node: UInt32, vertical: Bool, start: CGFloat, origin: CGFloat)?

    /// Invoked on link activation with the absolute href.
    var onLinkClick: ((String) -> Void)?
    /// Invoked when the link under the cursor changes (nil = none).
    var onStatusChange: ((String?) -> Void)?

    /// A11a: invoked with a page-space click that hit no link — the controller
    /// forwards it to the engine so a form control takes focus and the caret
    /// is placed. (Links keep the dedicated `onLinkClick` path.)
    var onClick: ((CGFloat, CGFloat) -> Void)?
    /// H5: invoked with a page-space ⌘-click — the element pick for the
    /// read-only inspector. Takes precedence over link handling.
    var onInspectClick: ((CGFloat, CGFloat) -> Void)?
    /// A11a: invoked with a run of typed text (composed/IME included) for the
    /// focused editable control.
    var onInsertText: ((String) -> Void)?
    /// A11a: invoked for caret movement / delete editing keys.
    var onKeyAction: ((CanvasKeyAction) -> Void)?
    /// Invoked for mouse/keyboard text-range operations.
    var onSelectionAction: ((CanvasSelectionAction) -> Void)?
    /// Invoked with the current IME marked text.
    var onMarkedText: ((String) -> Void)?
    /// Invoked when an IME composition commits, or clears it when `text` is nil.
    var onCompositionCommit: ((String?) -> Void)?
    /// Invoked when an IME composition is cancelled.
    var onCompositionCancel: (() -> Void)?
    /// Invoked for accessibility increment/decrement actions.
    var onStepFocus: ((Int) -> Void)?

    private var composing = false
    private var lastKeyModifiers: NSEvent.ModifierFlags = []
    private var pendingLink: PageLink?
    private var selectionDragging = false
    private var selectionOrigin: NSPoint?

    /// Cached Core Text lines for the current frame, keyed by text index.
    private var lineCache: [Int: CTLine] = [:]

    /// Cached per-character lines for letter-spaced runs (v10+, Q3), keyed by
    /// text index: one-char lines with their cumulative x offsets. Drawing at
    /// measure(char) + spacing advances reproduces the exact sum the engine
    /// laid the run out with.
    private var spacedCache: [Int: [(char: CTLine, x: CGFloat)]] = [:]

    /// GPU mode (Phase L5): skip backgrounds/layers (the Metal view behind
    /// paints them) and draw only text runs + scrollbar over transparency.
    var textOnly = false {
        didSet {
            if textOnly != oldValue {
                needsDisplay = true
            }
        }
    }

    /// Fires after view-local scrolling (the GPU tile follows the window).
    var onScrollChange: (() -> Void)?

    /// Decoded rasters for the current frame, keyed by payload id (avoids
    /// re-fetching and re-uploading pixels on every repaint). Cleared with
    /// the line cache when a new frame lands.
    private var rasterCache: [UInt64: CGImage] = [:]

    /// Animation players (Q8), keyed by payload id. The engine decodes but
    /// never plays (N8: no engine timeline) — this view steps frames on a
    /// CADisplayLink-paced clock, re-rendering only when a frame advances.
    private var animPlayers: [UInt64: AnimPlayer] = [:]
    private var animDisplayLink: CADisplayLink?
    private var lastLinkTs: CFTimeInterval = 0

    // MARK: Sizing

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    deinit {
        scrollTimer?.invalidate()
        animDisplayLink?.invalidate()
    }

    override var acceptsFirstResponder: Bool { true }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        // Hover feedback needs mouse-moved events from the window.
        window?.acceptsMouseMovedEvents = true
        if pageFrame != nil {
            rebuildAxElements()
        }
    }

    override var isFlipped: Bool { true }

    var contentHeight: CGFloat {
        CGFloat(pageFrame?.contentHeight ?? 0)
    }

    func maxScrollOffset() -> CGFloat {
        max(0, contentHeight - bounds.height)
    }

    // MARK: Frame updates

    func applyFrame(_ newFrame: EngineFrame) {
        scrollTimer?.invalidate()
        scrollTimer = nil
        let oldUrl = pageFrame?.url
        pageFrame = newFrame
        lineCache.removeAll(keepingCapacity: true)
        spacedCache.removeAll(keepingCapacity: true)
        rasterCache.removeAll(keepingCapacity: true)
        animPlayers.removeAll(keepingCapacity: true)
        syncAnimationClock()

        // A new document (not a resize repaint) snaps back to the top.
        if newFrame.url != oldUrl {
            scrollOffset = 0
            containerOffsets.removeAll(keepingCapacity: true)
            containerXOffsets.removeAll(keepingCapacity: true)
            pendingLink = nil
            selectionDragging = false
            selectionOrigin = nil
        } else {
            let nodes = Set(newFrame.scrollContainers.map(\.node))
            containerOffsets = containerOffsets.filter { nodes.contains($0.key) }
            containerXOffsets = containerXOffsets.filter { nodes.contains($0.key) }
        }
        clampScroll()
        rebuildAxElements()
        // H4: diff aria-live labels against the previous frame and announce
        // changes (after the AX cache rebuild so clients see fresh nodes).
        announceLiveRegionChanges()
        needsDisplay = true
        window?.invalidateCursorRects(for: self)
    }

    /// H3: the offscreen print copy is never torn down by a window, so it
    /// must not keep a display link (or frame caches) ticking after export.
    func stopAnimationClock() {
        scrollTimer?.invalidate()
        scrollTimer = nil
        animDisplayLink?.invalidate()
        animDisplayLink = nil
        animPlayers.removeAll()
        rasterCache.removeAll()
        lineCache.removeAll()
        spacedCache.removeAll()
        containerOffsets.removeAll()
        containerXOffsets.removeAll()
        scrollbarDrag = nil
        pageFrame = nil
    }

    func clearPageCaches() {
        scrollTimer?.invalidate()
        scrollTimer = nil
        animDisplayLink?.invalidate()
        animDisplayLink = nil
        animPlayers.removeAll(keepingCapacity: false)
        rasterCache.removeAll(keepingCapacity: false)
        lineCache.removeAll(keepingCapacity: false)
        spacedCache.removeAll(keepingCapacity: false)
        scrollbarDrag = nil
        needsDisplay = true
    }

    // MARK: Scrolling

    func setSmoothScrolling(_ enabled: Bool) {
        smoothScrolling = enabled
        if !enabled {
            scrollTimer?.invalidate()
            scrollTimer = nil
        }
    }

    private func maxContainerYOffset(_ container: PageScrollContainer) -> CGFloat {
        max(0, CGFloat(container.contentHeight) - CGFloat(container.h))
    }

    private func maxContainerXOffset(_ container: PageScrollContainer) -> CGFloat {
        max(0, CGFloat(container.contentWidth) - CGFloat(container.w))
    }

    private func containerArea(_ container: PageScrollContainer) -> CGFloat {
        max(0, CGFloat(container.w)) * max(0, CGFloat(container.h))
    }

    private func pageRect(_ container: PageScrollContainer) -> NSRect {
        NSRect(x: CGFloat(container.x), y: CGFloat(container.y),
               width: CGFloat(container.w), height: CGFloat(container.h))
    }

    private func contains(_ container: PageScrollContainer, point: NSPoint) -> Bool {
        pageRect(container).contains(point)
    }

    private func ancestorTranslation(for container: PageScrollContainer) -> CGPoint {
        guard let frame = pageFrame else { return .zero }
        let center = NSPoint(x: CGFloat(container.x + container.w / 2),
                             y: CGFloat(container.y + container.h / 2))
        let ancestors = frame.scrollContainers
            .filter { $0.node != container.node && contains($0, point: center) }
            .sorted { containerArea($0) > containerArea($1) }
        return ancestors.reduce(.zero) { result, ancestor in
            CGPoint(x: result.x + containerXOffsets[ancestor.node, default: 0],
                    y: result.y + containerOffsets[ancestor.node, default: 0])
        }
    }

    private func visibleRect(_ container: PageScrollContainer) -> NSRect {
        let ancestor = ancestorTranslation(for: container)
        return NSRect(x: CGFloat(container.x) - ancestor.x,
                      y: CGFloat(container.y) - scrollOffset - ancestor.y,
                      width: CGFloat(container.w), height: CGFloat(container.h))
    }

    private func containerChain(at point: NSPoint) -> [PageScrollContainer] {
        guard let frame = pageFrame else { return [] }
        return frame.scrollContainers
            .filter { visibleRect($0).contains(point) }
            .sorted { containerArea($0) < containerArea($1) }
    }

    private func containers(containing box: NSRect) -> [PageScrollContainer] {
        guard let frame = pageFrame else { return [] }
        let center = NSPoint(x: box.midX, y: box.midY)
        return frame.scrollContainers
            .filter { contains($0, point: center) }
            .sorted { containerArea($0) > containerArea($1) }
    }

    private func containerTranslation(for box: NSRect) -> CGPoint {
        containers(containing: box).reduce(.zero) { result, container in
            CGPoint(x: result.x + containerXOffsets[container.node, default: 0],
                    y: result.y + containerOffsets[container.node, default: 0])
        }
    }

    private func containerOffset(for box: NSRect) -> CGFloat {
        containerTranslation(for: box).y
    }

    private func applyContainerScroll(for box: NSRect, in ctx: CGContext,
                                      translatingBackground: Bool = true) {
        let chain = containers(containing: box)
        let ownNode = chain.first {
            pageRect($0).origin == box.origin && pageRect($0).size == box.size
        }?.node
        var appliedOffset = CGPoint.zero
        for container in chain {
            let clip = pageRect(container)
            ctx.clip(to: CGRect(x: clip.minX - appliedOffset.x,
                                y: clip.minY - appliedOffset.y,
                                width: clip.width, height: clip.height))
            let offset = CGPoint(x: containerXOffsets[container.node, default: 0],
                                 y: containerOffsets[container.node, default: 0])
            if translatingBackground || container.node != ownNode {
                ctx.translateBy(x: -offset.x, y: -offset.y)
                appliedOffset.x += offset.x
                appliedOffset.y += offset.y
            }
        }
    }

    private func setContainerOffsets(x: CGFloat, y: CGFloat, for container: PageScrollContainer) {
        let clampedX = min(max(0, x), maxContainerXOffset(container))
        let clampedY = min(max(0, y), maxContainerYOffset(container))
        guard containerXOffsets[container.node] != clampedX ||
                containerOffsets[container.node] != clampedY else { return }
        containerXOffsets[container.node] = clampedX
        containerOffsets[container.node] = clampedY
        rebuildAxElements()
        needsDisplay = true
        window?.invalidateCursorRects(for: self)
    }

    private func scrollDocument(by delta: CGFloat, smooth: Bool) {
        let target = scrollOffset + delta
        if smooth {
            animateScroll(to: target)
        } else {
            setScroll(target)
        }
    }

    private func scrollContainers(byX dx: CGFloat, byY dy: CGFloat, at point: NSPoint) {
        var remainderX = dx
        var remainderY = dy
        for container in containerChain(at: point) {
            let beforeX = containerXOffsets[container.node, default: 0]
            let beforeY = containerOffsets[container.node, default: 0]
            let afterX = min(max(0, beforeX + remainderX), maxContainerXOffset(container))
            let afterY = min(max(0, beforeY + remainderY), maxContainerYOffset(container))
            setContainerOffsets(x: afterX, y: afterY, for: container)
            remainderX -= afterX - beforeX
            remainderY -= afterY - beforeY
            if abs(remainderX) < 0.01 && abs(remainderY) < 0.01 { return }
        }
        if abs(remainderY) > 0.01 {
            scrollDocument(by: remainderY, smooth: smoothScrolling)
        }
    }

    func scroll(by delta: CGFloat, at point: NSPoint? = nil) {
        guard let point else {
            scrollDocument(by: delta, smooth: smoothScrolling)
            return
        }
        scrollContainers(byX: 0, byY: delta, at: point)
    }

    func scroll(byX dx: CGFloat, byY dy: CGFloat, at point: NSPoint) {
        scrollContainers(byX: dx, byY: dy, at: point)
    }

    func scroll(by delta: CGFloat) {
        scrollDocument(by: delta, smooth: smoothScrolling)
    }

    private func animateScroll(to target: CGFloat) {
        let clamped = min(max(0, target), maxScrollOffset())
        guard clamped != scrollOffset else { return }
        scrollTimer?.invalidate()
        scrollStart = scrollOffset
        scrollTarget = clamped
        let started = Date()
        let timer = Timer(timeInterval: 1.0 / 120.0, repeats: true) { [weak self] timer in
            guard let self else {
                timer.invalidate()
                return
            }
            let progress = min(1, Date().timeIntervalSince(started) / 0.18)
            let eased = 1 - pow(1 - progress, 3)
            self.setScroll(self.scrollStart + (self.scrollTarget - self.scrollStart) * CGFloat(eased))
            if progress >= 1 {
                timer.invalidate()
                self.scrollTimer = nil
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        scrollTimer = timer
    }

    func setScroll(_ value: CGFloat) {
        let clamped = min(max(0, value), maxScrollOffset())
        guard clamped != scrollOffset else { return }
        scrollOffset = clamped
        rebuildAxElements()
        needsDisplay = true
        window?.invalidateCursorRects(for: self)
        onScrollChange?()
    }

    private func clampScroll() {
        setScroll(scrollOffset)
    }

    override func scrollWheel(with event: NSEvent) {
        var dx = event.scrollingDeltaX
        var dy = event.scrollingDeltaY
        if event.modifierFlags.contains(.shift) {
            if abs(dx) > abs(dy) {
                dy = dx
                dx = 0
            } else if abs(dy) > abs(dx) {
                dx = dy
                dy = 0
            }
        }
        let point = convert(event.locationInWindow, from: nil)
        scroll(byX: dx, byY: dy, at: point)
    }

    override func scrollPageUp(_ sender: Any?) {
        scroll(by: -bounds.height * 0.9)
    }

    override func scrollPageDown(_ sender: Any?) {
        scroll(by: bounds.height * 0.9)
    }

    override func scrollLineUp(_ sender: Any?) {
        scroll(by: -40)
    }

    override func scrollLineDown(_ sender: Any?) {
        scroll(by: 40)
    }

    override func scrollToBeginningOfDocument(_ sender: Any?) {
        setScroll(0)
    }

    override func scrollToEndOfDocument(_ sender: Any?) {
        setScroll(maxScrollOffset())
    }

    // MARK: Hit testing

    private func link(at point: NSPoint) -> PageLink? {
        guard let pageFrame else { return nil }
        let pagePoint = pagePoint(for: point)
        let px = Float(pagePoint.x)
        let py = Float(pagePoint.y)
        for link in pageFrame.links.reversed() {
            if link.contains(x: px, y: py) {
                return link
            }
        }
        return nil
    }

    override func resetCursorRects() {
        guard let pageFrame else { return }
        for link in pageFrame.links {
            let pageBox = link.pageBounds
            let translation = containerTranslation(for: pageBox)
            let rect = NSRect(x: pageBox.minX - translation.x,
                              y: pageBox.minY - scrollOffset - translation.y,
                              width: pageBox.width,
                              height: pageBox.height)
            if rect.intersects(bounds) {
                addCursorRect(rect.intersection(bounds), cursor: .pointingHand)
            }
        }
    }

    private func pagePoint(for point: NSPoint) -> NSPoint {
        let nested = containerChain(at: point).reduce(CGPoint.zero) { result, container in
            CGPoint(x: result.x + containerXOffsets[container.node, default: 0],
                    y: result.y + containerOffsets[container.node, default: 0])
        }
        return NSPoint(x: point.x + nested.x, y: point.y + scrollOffset + nested.y)
    }

    override func mouseMoved(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        onStatusChange?(link(at: point)?.href)
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        if event.modifierFlags.contains(.command) {
            let page = pagePoint(for: point)
            onInspectClick?(page.x, page.y)
            return
        }
        if let hit = scrollbarHit(at: point) {
            beginScrollbarDrag(hit, at: point)
            return
        }
        window?.makeFirstResponder(self)
        pendingLink = link(at: point)
        selectionDragging = false
        selectionOrigin = pendingLink == nil ? point : nil
        if pendingLink == nil {
            let page = pagePoint(for: point)
            onClick?(page.x, page.y)
        }
    }

    override func mouseDragged(with event: NSEvent) {
        if scrollbarDrag != nil {
            updateScrollbarDrag(to: convert(event.locationInWindow, from: nil))
            return
        }
        guard let origin = selectionOrigin else {
            pendingLink = nil
            return
        }
        let point = convert(event.locationInWindow, from: nil)
        if !selectionDragging {
            let dx = point.x - origin.x
            let dy = point.y - origin.y
            guard dx * dx + dy * dy >= 9 else { return }
            selectionDragging = true
            let page = pagePoint(for: origin)
            onSelectionAction?(.beginPoint(page.x, page.y))
        }
        let page = pagePoint(for: point)
        onSelectionAction?(.extendPoint(page.x, page.y))
    }

    override func mouseUp(with event: NSEvent) {
        if scrollbarDrag != nil {
            scrollbarDrag = nil
            return
        }
        if selectionDragging {
            onSelectionAction?(.end)
        } else if let pendingLink {
            onLinkClick?(pendingLink.href)
        }
        pendingLink = nil
        selectionDragging = false
        selectionOrigin = nil
    }

    // MARK: Keyboard (A11a)

    override func keyDown(with event: NSEvent) {
        lastKeyModifiers = event.modifierFlags
        // Route through the standard key bindings so IME composition, dead
        // keys, and the editing selectors below all fire from one place.
        interpretKeyEvents([event])
    }

    override func insertText(_ insertion: Any) {
        if let s = insertion as? String, !s.isEmpty {
            if composing {
                composing = false
                onCompositionCommit?(s)
            } else {
                onInsertText?(s)
            }
        }
    }

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        guard let text = string as? String else { return }
        composing = true
        onMarkedText?(text)
    }

    func unmarkText() {
        guard composing else { return }
        composing = false
        onCompositionCommit?(nil)
    }

    func insertText(_ insertion: Any, replacementRange: NSRange) {
        insertText(insertion)
    }

    func setMarkedText(_ string: String, selectedRange: NSRange) {
        setMarkedText(string, selectedRange: selectedRange, replacementRange: NSRange(location: NSNotFound, length: 0))
    }

    func hasMarkedText() -> Bool { composing }

    func markedRange() -> NSRange {
        composing ? NSRange(location: 0, length: 0) : NSRange(location: NSNotFound, length: 0)
    }

    func selectedRange() -> NSRange {
        NSRange(location: NSNotFound, length: 0)
    }

    func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.font]
    }

    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        bounds
    }

    func characterIndex(for point: NSPoint) -> Int { 0 }

    override func doCommand(by selector: Selector) {
        if selector == #selector(insertTab(_:)) {
            insertTab(nil)
        } else if selector == #selector(insertBacktab(_:)) {
            insertBacktab(nil)
        } else {
            super.doCommand(by: selector)
        }
    }

    override func cancelOperation(_ sender: Any?) {
        guard composing else { return }
        composing = false
        onCompositionCancel?()
    }

    override func deleteBackward(_ sender: Any?) { onKeyAction?(.backspace) }
    override func deleteForward(_ sender: Any?) { onKeyAction?(.deleteForward) }
    override func moveLeft(_ sender: Any?) {
        onKeyAction?(lastKeyModifiers.contains(.shift) ? .extendCaretLeft : .caretLeft)
    }
    override func moveRight(_ sender: Any?) {
        onKeyAction?(lastKeyModifiers.contains(.shift) ? .extendCaretRight : .caretRight)
    }
    override func moveToBeginningOfLine(_ sender: Any?) {
        onKeyAction?(lastKeyModifiers.contains(.shift) ? .extendCaretHome : .caretHome)
    }
    override func moveToEndOfLine(_ sender: Any?) {
        onKeyAction?(lastKeyModifiers.contains(.shift) ? .extendCaretEnd : .caretEnd)
    }
    // H4: roving focus — Tab/⇧Tab walk the engine's focusables (links,
    // controls, contenteditables) instead of leaving the page. The engine
    // republishes with the new `axFocusedId`, so AX clients follow along.
    override func insertTab(_ sender: Any?) { onKeyAction?(.focusNext) }
    override func insertBacktab(_ sender: Any?) { onKeyAction?(.focusPrev) }

    // MARK: Painting

    override func draw(_ dirtyRect: NSRect) {
        guard let pageFrame else {
            if !textOnly {
                Theme.canvasLight.setFill()
                bounds.fill()
            }
            return
        }

        // Page background: engine-provided when opaque, else the theme
        // canvas (which follows light/dark). Skipped in text-only mode —
        // the Metal bitmap behind already carries the layers.
        if !textOnly {
            if pageFrame.opaquePageBackground {
                EngineFrame.color(pageFrame.pageBackground).setFill()
            } else if isDarkAppearance {
                Theme.canvasDark.setFill()
            } else {
                Theme.canvasLight.setFill()
            }
            bounds.fill()
        }

        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        ctx.saveGState()
        // Page space → view space: shift up by the scroll offset.
        ctx.translateBy(x: 0, y: -scrollOffset)
        ctx.clip(to: CGRect(x: 0, y: scrollOffset, width: bounds.width, height: bounds.height))

        // Q5: sticky boxes paint at their flow position; the view shifts the
        // boxed items by the stick delta for the current scroll offset.
        let sticky = stickyShiftMaps()
        // B7 (v18): per-item clip-path/mask shapes, page space. Installed
        // inside each item's transform+shift frame, so the clip rides the
        // same CTM as the drawing it constrains. B8: the same tail carries
        // the mix-blend-mode id (applied per item) and the isolation group
        // — contiguous equal-group runs in each kind's loop paint inside
        // one transparency layer, so descendant blends composite within the
        // isolate instead of reaching the page backdrop. (Documented
        // approximations: fragments across kind loops are separate layers,
        // and the first item of a layer blends against a transparent
        // backdrop, so it reads close to normal.)
        let stamps = stampMaps()
        // B8 layer state machine: `openGroup` is the id of the transparency
        // layer currently capturing draws (0 = none). Transitions run at
        // each item's level, between per-item save/restore frames, so the
        // beginTransparencyLayer/endTransparencyLayer pair stays balanced
        // (the end call must happen at the same graphics-state level as
        // the begin call).
        var openGroup: UInt32 = 0
        func setGroup(_ g: UInt32) {
            guard g != openGroup else { return }
            if openGroup != 0 { ctx.endTransparencyLayer() }
            openGroup = g
            if g != 0 { ctx.beginTransparencyLayer(auxiliaryInfo: nil) }
        }

        if !textOnly {
            for (i, shadow) in pageFrame.shadows.enumerated() where !shadow.inset {
                setGroup(stamps.shadows.group[i] ?? 0)
                let box = NSRect(x: CGFloat(shadow.x), y: CGFloat(shadow.y),
                                 width: CGFloat(shadow.w), height: CGFloat(shadow.h))
                ctx.saveGState()
                applyContainerScroll(for: box, in: ctx)
                warp(shadow.transform)
                if sticky.shadows[i] != 0 { ctx.translateBy(x: 0, y: CGFloat(sticky.shadows[i])) }
                if let b = stamps.shadows.blend[i], b != 0 { ctx.setBlendMode(cgBlendMode(b)) }
                if let c = stamps.shadows.clip[i] { applyClip(c, ctx) }
                drawShadow(shadow)
                ctx.restoreGState()
            }

            for (i, rect) in pageFrame.rects.enumerated() {
                setGroup(stamps.rects.group[i] ?? 0)
                let box = NSRect(x: CGFloat(rect.x), y: CGFloat(rect.y),
                                 width: CGFloat(rect.w), height: CGFloat(rect.h))
                ctx.saveGState()
                applyContainerScroll(for: box, in: ctx, translatingBackground: false)
                warp(rect.transform)
                if sticky.rects[i] != 0 { ctx.translateBy(x: 0, y: CGFloat(sticky.rects[i])) }
                if let b = stamps.rects.blend[i], b != 0 { ctx.setBlendMode(cgBlendMode(b)) }
                if let c = stamps.rects.clip[i] { applyClip(c, ctx) }
                drawRect(rect)
                ctx.restoreGState()
            }
        }

        // Images: decoded rasters when the frame carries a payload id
        // (pulled through the engine's cache, kitae_qtsq_raster); the
        // placeholder box otherwise. Alt text already rides in `texts`.
        // Skipped in text-only mode (the Metal bitmap carries them).
        if !textOnly {
            for (i, im) in pageFrame.images.enumerated() {
                setGroup(stamps.images.group[i] ?? 0)
                let box = NSRect(x: CGFloat(im.x), y: CGFloat(im.y),
                                 width: CGFloat(im.w), height: CGFloat(im.h))
                ctx.saveGState()
                defer { ctx.restoreGState() }
                applyContainerScroll(for: box, in: ctx)
                warp(im.transform)
                if sticky.images[i] != 0 { ctx.translateBy(x: 0, y: CGFloat(sticky.images[i])) }
                if let b = stamps.images.blend[i], b != 0 { ctx.setBlendMode(cgBlendMode(b)) }
                if let c = stamps.images.clip[i] { applyClip(c, ctx) }
                if im.payload != 0 {
                    // Animated (Q8): the player shows the frame its clock is on.
                    if im.animated, let img = currentAnimFrame(im.payload) {
                        drawRaster(img, fit: im.fit, in: box)
                        continue
                    }
                    if let raster = rasterImage(im.payload) {
                        drawRaster(raster, fit: im.fit, in: box)
                        continue
                    }
                }
                NSColor(red: 0xE5/255.0, green: 0xE7/255.0, blue: 0xEB/255.0, alpha: 1).setFill()
                box.fill()
                NSColor(red: 0x9A/255.0, green: 0xA0/255.0, blue: 0xA6/255.0, alpha: 1).setStroke()
                let border = NSBezierPath(rect: box)
                border.lineWidth = 1
                border.stroke()
            }
        }

        for (index, run) in pageFrame.texts.enumerated() {
            setGroup(stamps.texts.group[index] ?? 0)
            let line = cachedLine(for: run, at: index)
            let lineBounds = CTLineGetBoundsWithOptions(line, .useGlyphPathBounds)
            let textBox = NSRect(x: CGFloat(run.x), y: CGFloat(run.y) - CGFloat(lineBounds.height),
                                 width: CGFloat(lineBounds.width), height: CGFloat(lineBounds.height))
            // Core Text draws with a y-up baseline; flip within the run.
            ctx.saveGState()
            applyContainerScroll(for: textBox, in: ctx)
            warp(run.transform)
            if sticky.texts[index] != 0 { ctx.translateBy(x: 0, y: CGFloat(sticky.texts[index])) }
            if let b = stamps.texts.blend[index], b != 0 { ctx.setBlendMode(cgBlendMode(b)) }
            if let c = stamps.texts.clip[index] { applyClip(c, ctx) }
            ctx.translateBy(x: CGFloat(run.x), y: CGFloat(run.y))
            ctx.scaleBy(x: 1, y: -1)
            ctx.textPosition = .zero
            if run.letterSpacing != 0 {
                // Letter-spaced run: per-char lines at measure(char) + spacing
                // advances — the same per-char widths the engine measured
                // through the vtable, so paint matches layout exactly.
                for (glyph, x) in cachedSpacedChars(for: run, at: index) {
                    ctx.textPosition = CGPoint(x: x, y: 0)
                    CTLineDraw(glyph, ctx)
                }
            } else {
                let line = cachedLine(for: run, at: index)
                CTLineDraw(line, ctx)
            }
            ctx.restoreGState()
        }
        setGroup(0) // flush any group still open before the overlay + restore

        // Find-in-page overlay (P1): dim wash over every match, strong
        // accent over the active one. Page space — the scroll translate
        // above still applies.
        if !pageFrame.findMatches.isEmpty {
            for (index, m) in pageFrame.findMatches.enumerated() {
                let active = (pageFrame.findActive == index)
                (active ? NSColor.selectedTextBackgroundColor.withAlphaComponent(0.85)
                        : NSColor.systemYellow.withAlphaComponent(0.45)).setFill()
                for r in m.rects {
                    NSRect(x: CGFloat(r.x), y: CGFloat(r.y),
                           width: CGFloat(r.w), height: CGFloat(r.h)).fill(using: .sourceOver)
                }
            }
        }

        if showDebugBounds { drawDebugBounds(in: ctx) }

        ctx.restoreGState()

        drawScrollbar()
    }

    private func drawDebugBounds(in ctx: CGContext) {
        guard let pageFrame else { return }
        ctx.saveGState()
        ctx.setLineWidth(0.5)
        ctx.setStrokeColor(NSColor.systemRed.cgColor)
        for rect in pageFrame.rects {
            ctx.stroke(NSRect(x: CGFloat(rect.x), y: CGFloat(rect.y),
                               width: CGFloat(rect.w), height: CGFloat(rect.h)))
        }
        ctx.setStrokeColor(NSColor.systemGreen.cgColor)
        for image in pageFrame.images {
            ctx.stroke(NSRect(x: CGFloat(image.x), y: CGFloat(image.y),
                               width: CGFloat(image.w), height: CGFloat(image.h)))
        }
        ctx.setStrokeColor(NSColor.systemBlue.cgColor)
        for (index, run) in pageFrame.texts.enumerated() {
            let line = cachedLine(for: run, at: index)
            let bounds = CTLineGetBoundsWithOptions(line, .useGlyphPathBounds)
            ctx.saveGState()
            warp(run.transform)
            ctx.stroke(NSRect(x: CGFloat(run.x), y: CGFloat(run.y),
                               width: CGFloat(bounds.width), height: CGFloat(bounds.height)))
            ctx.restoreGState()
        }
        ctx.restoreGState()
    }

    // MARK: Paint upgrades (rounded rects, gradients, shadows)

    /// Q5: per-item paint shift from the sticky side-list. Each sticky box's
    /// painted items move together by its stick delta at the current scroll;
    /// items outside any sticky box shift by zero.
    private func stickyShiftMaps()
        -> (rects: [Float], texts: [Float], images: [Float], shadows: [Float]) {
        var r = [Float](repeating: 0, count: pageFrame?.rects.count ?? 0)
        var t = [Float](repeating: 0, count: pageFrame?.texts.count ?? 0)
        var im = [Float](repeating: 0, count: pageFrame?.images.count ?? 0)
        var sh = [Float](repeating: 0, count: pageFrame?.shadows.count ?? 0)
        guard let pageFrame, !pageFrame.stickies.isEmpty else { return (r, t, im, sh) }
        let sy = Float(scrollOffset)
        let vh = Float(bounds.height)
        func fill(_ map: inout [Float], _ range: (UInt32, UInt32), _ dy: Float) {
            let start = Int(range.0)
            let end = min(Int(range.0 &+ range.1), map.count)
            guard start < end else { return }
            for i in start..<end { map[i] = dy }
        }
        for s in pageFrame.stickies {
            let dy = s.shift(scrollY: sy, viewportHeight: vh)
            guard dy != 0 else { continue }
            fill(&r, s.rect, dy)
            fill(&t, s.text, dy)
            fill(&im, s.image, dy)
            fill(&sh, s.shadow, dy)
        }
        return (r, t, im, sh)
    }

    // MARK: Clip / blend / isolation stamps (B7+B8, v18)

    /// v18: per-kind clip/blend/group lookup maps for the draw loops. The
    /// wire tail is sparse; only items stamped by a B7 clip-path/mask or a
    /// B8 mix-blend-mode/isolation are present, and stale indices are
    /// dropped here so painters can subscript freely.
    private struct ItemStamps {
        var clip: [Int: PageClip] = [:]
        var blend: [Int: UInt8] = [:]
        var group: [Int: UInt32] = [:]
    }

    private func stampMaps()
        -> (rects: ItemStamps, texts: ItemStamps,
            images: ItemStamps, shadows: ItemStamps) {
        var r = ItemStamps()
        var t = ItemStamps()
        var im = ItemStamps()
        var sh = ItemStamps()
        guard let pageFrame, !pageFrame.clips.isEmpty else { return (r, t, im, sh) }
        for e in pageFrame.clips {
            switch e.kind {
            case 0 where e.index < pageFrame.rects.count:
                r.clip[e.index] = e.clip; r.blend[e.index] = e.blend; r.group[e.index] = e.group
            case 1 where e.index < pageFrame.texts.count:
                t.clip[e.index] = e.clip; t.blend[e.index] = e.blend; t.group[e.index] = e.group
            case 2 where e.index < pageFrame.images.count:
                im.clip[e.index] = e.clip; im.blend[e.index] = e.blend; im.group[e.index] = e.group
            case 3 where e.index < pageFrame.shadows.count:
                sh.clip[e.index] = e.clip; sh.blend[e.index] = e.blend; sh.group[e.index] = e.group
            default: break
            }
        }
        return (r, t, im, sh)
    }

    /// B8: wire mix-blend-mode id → CoreGraphics blend mode. The table is
    /// the canonical one in kitae_core.h (0 normal … 15 luminosity); CG
    /// exposes every CSS separable and non-separable mode.
    private func cgBlendMode(_ id: UInt8) -> CGBlendMode {
        switch id {
        case 1: return .multiply
        case 2: return .screen
        case 3: return .overlay
        case 4: return .darken
        case 5: return .lighten
        case 6: return .colorDodge
        case 7: return .colorBurn
        case 8: return .hardLight
        case 9: return .softLight
        case 10: return .difference
        case 11: return .exclusion
        case 12: return .hue
        case 13: return .saturation
        case 14: return .color
        case 15: return .luminosity
        default: return .normal
        }
    }

    /// v18: install a page-space clip shape as the context's clip. Call it
    /// inside the item's warp + sticky-shift frame (before any run-local
    /// translate) so the clip rides the same CTM as the drawing it bounds.
    /// Elliptical inset corners (hr ≠ vr) draw as circular with the larger
    /// radius — asymmetric elliptical `round` authoring is rare and the
    /// engine already clamps each corner to the box.
    private func applyClip(_ c: PageClip, _ ctx: CGContext) {
        switch c {
        case let .circle(cx, cy, r):
            let path = CGMutablePath()
            path.addEllipse(in: CGRect(x: CGFloat(cx - r), y: CGFloat(cy - r),
                                       width: CGFloat(r) * 2, height: CGFloat(r) * 2))
            ctx.addPath(path)
            ctx.clip()
        case let .ellipse(cx, cy, rx, ry):
            let path = CGMutablePath()
            path.addEllipse(in: CGRect(x: CGFloat(cx - rx), y: CGFloat(cy - ry),
                                       width: CGFloat(rx) * 2, height: CGFloat(ry) * 2))
            ctx.addPath(path)
            ctx.clip()
        case let .inset(x, y, w, h, radii):
            let path = CGMutablePath()
            addInsetPath(path, box: CGRect(x: CGFloat(x), y: CGFloat(y),
                                           width: CGFloat(w), height: CGFloat(h)),
                         radii: radii)
            ctx.addPath(path)
            ctx.clip()
        case let .polygon(evenOdd, points):
            guard let first = points.first else { return } // degenerate: no clip
            if evenOdd {
                // Self-intersecting stars need the even-odd rule; CG's path
                // clip is nonzero-only, but NSBezierPath carries winding
                // state that setClip honors.
                let p = NSBezierPath()
                p.windingRule = .evenOdd
                p.move(to: NSPoint(x: CGFloat(first.0), y: CGFloat(first.1)))
                for pt in points.dropFirst() {
                    p.line(to: NSPoint(x: CGFloat(pt.0), y: CGFloat(pt.1)))
                }
                p.close()
                p.setClip()
            } else {
                let path = CGMutablePath()
                path.move(to: CGPoint(x: CGFloat(first.0), y: CGFloat(first.1)))
                for pt in points.dropFirst() {
                    path.addLine(to: CGPoint(x: CGFloat(pt.0), y: CGFloat(pt.1)))
                }
                path.closeSubpath()
                ctx.addPath(path)
                ctx.clip()
            }
        }
    }

    /// Rounded-rect path for an inset clip. Corner order matches the wire
    /// (tl, tr, br, bl) and the arc angles mirror `roundedPath`'s NSBezier
    /// convention in this y-down page space.
    private func addInsetPath(_ path: CGMutablePath, box: CGRect, radii: [(Float, Float)]) {
        let cap = min(box.width, box.height) / 2
        let rr = radii.map { max(0, min(CGFloat(max($0.0, $0.1)), cap)) }
        guard rr.contains(where: { $0 > 0 }) else { path.addRect(box); return }
        let (x0, y0, x1, y1) = (box.minX, box.minY, box.maxX, box.maxY)
        func deg(_ d: CGFloat) -> CGFloat { d * .pi / 180 }
        path.move(to: CGPoint(x: x0 + rr[0], y: y0))
        path.addLine(to: CGPoint(x: x1 - rr[1], y: y0))
        path.addArc(center: CGPoint(x: x1 - rr[1], y: y0 + rr[1]), radius: rr[1],
                    startAngle: deg(270), endAngle: deg(360), clockwise: false)
        path.addLine(to: CGPoint(x: x1, y: y1 - rr[2]))
        path.addArc(center: CGPoint(x: x1 - rr[2], y: y1 - rr[2]), radius: rr[2],
                    startAngle: deg(0), endAngle: deg(90), clockwise: false)
        path.addLine(to: CGPoint(x: x0 + rr[3], y: y1))
        path.addArc(center: CGPoint(x: x0 + rr[3], y: y1 - rr[3]), radius: rr[3],
                    startAngle: deg(90), endAngle: deg(180), clockwise: false)
        path.addLine(to: CGPoint(x: x0, y: y0 + rr[0]))
        path.addArc(center: CGPoint(x: x0 + rr[0], y: y0 + rr[0]), radius: rr[0],
                    startAngle: deg(180), endAngle: deg(270), clockwise: false)
        path.closeSubpath()
    }

    /// Concatenates an item's affine warp into the CTM (Q2). The engine
    /// stores the matrix in page space and paints items in local space, so
    /// callers bracket the item's own painting with save/restore and draw
    /// exactly as untransformed — the CTM carries the warp, matching the
    /// native and GPU painters' convention.
    private func warp(_ m: PageTransform?) {
        guard let m else { return }
        // CGAffineTransform rows: x' = a·x + c·y + tx, y' = b·x + d·y + ty;
        // the engine's 2×3 is x' = m0·x + m1·y + m2, y' = m3·x + m4·y + m5.
        NSGraphicsContext.current?.cgContext.concatenate(
            CGAffineTransform(a: CGFloat(m.m0), b: CGFloat(m.m3),
                              c: CGFloat(m.m1), d: CGFloat(m.m4),
                              tx: CGFloat(m.m2), ty: CGFloat(m.m5)))
    }

    /// Rounded-rectangle path with per-corner radii (clamped to the box).
    private func roundedPath(_ rect: NSRect, radius r: (Float, Float, Float, Float)) -> NSBezierPath {
        let radii = [CGFloat(r.0), CGFloat(r.1), CGFloat(r.2), CGFloat(r.3)]
            .map { min(max($0, 0), min(rect.width, rect.height) / 2) }
        guard radii.contains(where: { $0 > 0 }) else { return NSBezierPath(rect: rect) }
        // Corners in AppKit order: TL, TR, BR, BL (view is flipped).
        let (x0, y0, x1, y1) = (rect.minX, rect.minY, rect.maxX, rect.maxY)
        let path = NSBezierPath()
        path.move(to: NSPoint(x: x0 + radii[0], y: y0))
        path.line(to: NSPoint(x: x1 - radii[1], y: y0))
        path.appendArc(withCenter: NSPoint(x: x1 - radii[1], y: y0 + radii[1]),
                       radius: radii[1], startAngle: 270, endAngle: 0)
        path.line(to: NSPoint(x: x1, y: y1 - radii[2]))
        path.appendArc(withCenter: NSPoint(x: x1 - radii[2], y: y1 - radii[2]),
                       radius: radii[2], startAngle: 0, endAngle: 90)
        path.line(to: NSPoint(x: x0 + radii[3], y: y1))
        path.appendArc(withCenter: NSPoint(x: x0 + radii[3], y: y1 - radii[3]),
                       radius: radii[3], startAngle: 90, endAngle: 180)
        path.line(to: NSPoint(x: x0, y: y0 + radii[0]))
        path.appendArc(withCenter: NSPoint(x: x0 + radii[0], y: y0 + radii[0]),
                       radius: radii[0], startAngle: 180, endAngle: 270)
        path.close()
        return path
    }

    private func drawRect(_ rect: PageRect) {
        let box = NSRect(x: CGFloat(rect.x), y: CGFloat(rect.y),
                         width: CGFloat(rect.w), height: CGFloat(rect.h))
        guard box.width > 0, box.height > 0 else { return }
        let path = roundedPath(box, radius: rect.radius)
        switch rect.paint {
        case .solid:
            EngineFrame.color(rect.color).setFill()
            path.fill()
        case .linear(let angle, let stops):
            guard let gradient = makeGradient(stops), let ctx = NSGraphicsContext.current?.cgContext
            else {
                EngineFrame.color(rect.color).setFill()
                path.fill()
                return
            }
            ctx.saveGState()
            path.addClip()
            // CSS degrees run clockwise from "to top"; map to a direction.
            let rad = CGFloat(angle) * .pi / 180
            let dir = CGVector(dx: sin(rad), dy: cos(rad))
            let center = CGPoint(x: box.midX, y: box.midY)
            let half = (abs(box.width * dir.dx) + abs(box.height * dir.dy)) / 2
            ctx.drawLinearGradient(
                gradient,
                start: CGPoint(x: center.x - dir.dx * half, y: center.y - dir.dy * half),
                end: CGPoint(x: center.x + dir.dx * half, y: center.y + dir.dy * half),
                options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
            ctx.restoreGState()
        case .radial(let cx, let cy, let radius, let stops):
            guard let gradient = makeGradient(stops), let ctx = NSGraphicsContext.current?.cgContext
            else {
                EngineFrame.color(rect.color).setFill()
                path.fill()
                return
            }
            let center = CGPoint(x: box.minX + CGFloat(cx) * box.width,
                                 y: box.minY + CGFloat(cy) * box.height)
            let far = sqrt(pow(max(center.x - box.minX, box.maxX - center.x), 2)
                         + pow(max(center.y - box.minY, box.maxY - center.y), 2))
            ctx.saveGState()
            path.addClip()
            ctx.drawRadialGradient(
                gradient,
                startCenter: center, startRadius: 0,
                endCenter: center, endRadius: radius.map { CGFloat($0) } ?? far,
                options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
            ctx.restoreGState()
        case .conic(let angle, let cx, let cy, let stops):
            // B5: angular sweep around the center — approximated with 360
            // thin wedges (CoreGraphics has no conic gradient).
            guard let ctx = NSGraphicsContext.current?.cgContext, !stops.isEmpty else {
                EngineFrame.color(rect.color).setFill()
                path.fill()
                return
            }
            let center = CGPoint(x: box.minX + CGFloat(cx) * box.width,
                                 y: box.minY + CGFloat(cy) * box.height)
            let reach = max(box.width, box.height) * 2
            ctx.saveGState()
            path.addClip()
            let point = { (deg: Float) -> CGPoint in
                let rad = CGFloat(deg) * .pi / 180
                return CGPoint(x: center.x + sin(rad) * reach,
                               y: center.y - cos(rad) * reach)
            }
            for step in 0..<360 {
                if let c = colorAt(stops, Float(step) / 360) {
                    let a0 = angle + Float(step)
                    let wedge = CGMutablePath()
                    wedge.move(to: center)
                    wedge.addLine(to: point(a0))
                    wedge.addLine(to: point(a0 + 1.2))
                    wedge.closeSubpath()
                    ctx.setFillColor(EngineFrame.color(c).cgColor)
                    ctx.addPath(wedge)
                    ctx.fillPath()
                }
            }
            ctx.restoreGState()
        case .image:
            // Background-image raster paint is future work: fallback color.
            EngineFrame.color(rect.color).setFill()
            path.fill()
        }
    }

    private func makeGradient(_ stops: [(offset: Float, color: UInt32)]) -> CGGradient? {
        guard !stops.isEmpty else { return nil }
        let colors = stops.map { EngineFrame.color($0.color) as AnyObject }
        let locations = stops.map { CGFloat($0.offset) }
        return CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                          colors: colors as CFArray,
                          locations: locations)
    }

    /// B5 conic: sample the stop list at sweep fraction `t`, interpolating
    /// between the bracketing stops (mirrors the C++ `GradientAt`).
    private func colorAt(_ stops: [(offset: Float, color: UInt32)], _ t: Float) -> UInt32? {
        guard let first = stops.first else { return nil }
        if t <= first.offset { return first.color }
        for (a, b) in zip(stops, stops.dropFirst()) where t <= b.offset {
            let span = max(b.offset - a.offset, 0.0001)
            return lerpColor(a.color, b.color, CGFloat((t - a.offset) / span))
        }
        return stops.last?.color
    }

    private func lerpColor(_ c0: UInt32, _ c1: UInt32, _ k: CGFloat) -> UInt32 {
        func ch(_ c: UInt32, _ shift: UInt32) -> CGFloat { CGFloat((c >> shift) & 0xFF) }
        func mix(_ a: CGFloat, _ b: CGFloat) -> UInt32 { UInt32((a + (b - a) * k).rounded()) }
        return (mix(ch(c0, 24), ch(c1, 24)) << 24) | (mix(ch(c0, 16), ch(c1, 16)) << 16)
             | (mix(ch(c0, 8), ch(c1, 8)) << 8) | mix(ch(c0, 0), ch(c1, 0))
    }

    private func drawShadow(_ shadow: PageShadow) {
        let base = NSRect(x: CGFloat(shadow.x), y: CGFloat(shadow.y),
                          width: CGFloat(shadow.w), height: CGFloat(shadow.h))
        guard base.width > 0, base.height > 0 else { return }
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let expanded = base.insetBy(dx: CGFloat(-shadow.spread), dy: CGFloat(-shadow.spread))
        let path = roundedPath(expanded, radius: shadow.radius)
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: CGFloat(shadow.offsetX), height: CGFloat(shadow.offsetY)),
                      blur: CGFloat(max(shadow.blur, 0)),
                      color: EngineFrame.color(shadow.color).cgColor)
        // Fill with anything: only the shadow halo survives outside the path.
        EngineFrame.color(shadow.color).setFill()
        path.fill()
        ctx.restoreGState()
    }

    /// E3: a blurred run is an R8 text-shadow duplicate — the engine already
    /// baked the shadow's offset into the run position and its color into
    /// `run.color`, so the painter draws only the blurred silhouette:
    /// transparent foreground + an offset-zero NSShadow of the run's color
    /// (Core Graphics renders just the blurred glyph shape).
    private func shadowAttrs(for run: PageText) -> [NSAttributedString.Key: Any] {
        guard run.shadowBlur > 0 else { return [:] }
        let sh = NSShadow()
        sh.shadowColor = EngineFrame.color(run.color)
        sh.shadowBlurRadius = CGFloat(run.shadowBlur)
        sh.shadowOffset = .zero
        return [.shadow: sh,
                .foregroundColor: EngineFrame.color(run.color).withAlphaComponent(0)]
    }

    private func cachedLine(for run: PageText, at index: Int) -> CTLine {
        if let cached = lineCache[index] { return cached }
        let font = CoreTextMeasurer.shared.font(size: run.size, weight: run.weight,
                                                italic: run.italic, monospace: run.monospace,
                                                family: run.family)
        var attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: EngineFrame.color(run.color),
        ]
        attrs.merge(shadowAttrs(for: run)) { _, new in new }
        let attributed = NSAttributedString(string: run.text, attributes: attrs)
        let line = CTLineCreateWithAttributedString(attributed)
        lineCache[index] = line
        return line
    }

    /// Per-character lines with cumulative x offsets for a letter-spaced run
    /// (Q3). The last glyph gets no trailing spacing, matching
    /// `TextMeasurer::measure_run_sp` and the native/GPU painters.
    private func cachedSpacedChars(for run: PageText, at index: Int)
        -> [(char: CTLine, x: CGFloat)] {
        if let cached = spacedCache[index] { return cached }
        let font = CoreTextMeasurer.shared.font(size: run.size, weight: run.weight,
                                                italic: run.italic, monospace: run.monospace,
                                                family: run.family)
        let color = EngineFrame.color(run.color)
        let spacing = CGFloat(run.letterSpacing)
        var chars: [(char: CTLine, x: CGFloat)] = []
        chars.reserveCapacity(run.text.count)
        var cx: CGFloat = 0
        let count = run.text.count
        let shadow = shadowAttrs(for: run)
        for (i, ch) in run.text.enumerated() {
            let s = String(ch)
            var attrs: [NSAttributedString.Key: Any] = [
                .font: font,
                .foregroundColor: color,
            ]
            attrs.merge(shadow) { _, new in new }
            let attributed = NSAttributedString(string: s, attributes: attrs)
            let line = CTLineCreateWithAttributedString(attributed)
            chars.append((line, cx))
            if i + 1 < count {
                cx += CGFloat(CoreTextMeasurer.shared.measure(
                    s, size: run.size, weight: run.weight, italic: run.italic,
                    monospace: run.monospace, family: run.family)) + spacing
            }
        }
        spacedCache[index] = chars
        return chars
    }

    // MARK: Decoded image rasters (J3)

    /// Target proxy for the animation display link: keeps the view weak so a
    /// removed view's link can't retain (or fire into) a dead view.
    fileprivate final class WeakProxy: NSObject {
        private weak var target: PageCanvasView?
        init(_ target: PageCanvasView) { self.target = target }
        @objc func animationTick(_ link: CADisplayLink) {
            target?.animationTick(link)
        }
    }

    /// Per-image animation state (Q8). Holds the decoded frame images and
    /// advances `frameIndex`/`elapsed` as the display link ticks.
    private final class AnimPlayer {
        let frames: [CGImage]
        let delays: [Double] // seconds
        let loopsForever: Bool
        /// Presentation state.
        var index = 0
        var elapsed: Double = 0
        /// Whether the current index differs from what was last painted.
        var dirty = true
        init(frames: [CGImage], delays: [Double], loopsForever: Bool) {
            self.frames = frames
            self.delays = delays
            self.loopsForever = loopsForever
        }
        /// Advance by `dt` seconds. Returns true when the visible frame changed.
        func tick(_ dt: Double) -> Bool {
            guard frames.count > 1 else { return false }
            elapsed += dt
            while elapsed >= delays[index] {
                elapsed -= delays[index]
                index += 1
                if index >= frames.count {
                    if loopsForever {
                        index = 0
                    } else {
                        index = frames.count - 1
                        elapsed = 0
                        return false
                    }
                }
                dirty = true
            }
            if dirty {
                dirty = false
                return true
            }
            return false
        }
    }

    /// Rebuild players for the frame's animated images and start/stop the
    /// display link. Frames decode lazily on first paint of each index.
    private func syncAnimationClock() {
        animPlayers.removeAll(keepingCapacity: true)
        for im in pageFrame?.images ?? [] where im.animated && im.payload != 0 {
            let count = kitae_raster_frames(im.payload)
            guard count > 1 else { continue }
            var images: [CGImage] = []
            var delays: [Double] = []
            images.reserveCapacity(Int(count))
            for n in 0..<count {
                if let img = animFrameImage(im.payload, n) {
                    images.append(img)
                    delays.append(max(0.02, Double(kitae_raster_frame_delay(im.payload, n)) / 1000.0))
                }
            }
            guard images.count > 1 else { continue }
            // 65535 = forever (NETSCAPE infinite); anything else plays its
            // declared count (0 = once).
            let loopsForever = kitae_raster_loop(im.payload) == 65535
            animPlayers[im.payload] = AnimPlayer(frames: images, delays: delays, loopsForever: loopsForever)
        }
        if animPlayers.isEmpty {
            animDisplayLink?.invalidate()
            animDisplayLink = nil
            lastLinkTs = 0
        } else if animDisplayLink == nil {
            // macOS: display links come from NSView, not CADisplayLink's
            // (iOS-only) target/selector constructor.
            let link = self.displayLink(target: WeakProxy(self),
                                        selector: #selector(WeakProxy.animationTick))
            link.add(to: .main, forMode: .common)
            animDisplayLink = link
        }
    }

    /// CADisplayLink callback: advance every player, repaint on changes.
    fileprivate func animationTick(_ link: CADisplayLink) {
        let ts = link.timestamp
        let dt = lastLinkTs == 0 ? 0 : min(0.25, ts - lastLinkTs)
        lastLinkTs = ts
        var anyChanged = false
        for player in animPlayers.values {
            if player.tick(dt) {
                anyChanged = true
            }
        }
        if anyChanged {
            needsDisplay = true
        }
    }

    /// The frame image a player currently shows for `payload` (nil = use the
    /// still). Called from the paint loop instead of `rasterImage` when the
    /// image is animated.
    fileprivate func currentAnimFrame(_ payload: UInt64) -> CGImage? {
        animPlayers[payload]?.frames[animPlayers[payload]!.index]
    }

    /// Pull one animation frame via the C ABI (same wire layout as the
    /// still raster; premultiplication identical).
    private func animFrameImage(_ payload: UInt64, _ n: UInt32) -> CGImage? {
        let total = kitae_qtsq_raster_size(payload)
        guard total >= 8 else { return nil }
        var buf = [UInt8](repeating: 0, count: total)
        guard kitae_raster_frame(payload, n, &buf, buf.count) == 1 else { return nil }
        return Self.rasterCGImage(from: buf)
    }

    /// Shared decode of a wire raster buffer (w/h header + RGBA8) into a
    /// premultiplied CGImage.
    private static func rasterCGImage(from buf: [UInt8]) -> CGImage? {
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

    /// Pulls a decoded raster out of the engine's side-table via the C ABI:
    /// u32 width, u32 height, then w*h*4 bytes RGBA8 (row-major, straight
    /// alpha, top row first — matches the canvas's flipped page space).
    private func rasterImage(_ payload: UInt64) -> CGImage? {
        if let cached = rasterCache[payload] { return cached }
        let total = kitae_qtsq_raster_size(payload)
        guard total >= 8 else { return nil }
        var buf = [UInt8](repeating: 0, count: total)
        guard kitae_qtsq_raster(payload, &buf, buf.count) == 1 else { return nil }
        guard let image = Self.rasterCGImage(from: buf) else { return nil }
        rasterCache[payload] = image
        return image
    }

    /// Draws a decoded image inside its layout box honoring object-fit.
    private func drawRaster(_ image: CGImage, fit: UInt32, in box: NSRect) {
        guard box.width > 0, box.height > 0 else { return }
        let src = CGSize(width: image.width, height: image.height)
        var dest = box
        // Scale to fit, then apply the fit mode. "fill" stretches; the rest
        // preserve aspect ratio (cover additionally clips).
        if src.width > 0, src.height > 0 {
            let scale: CGFloat
            switch fit {
            case 0: scale = 1 // fill stretches below
            case 2, 3: scale = max(box.width / src.width, box.height / src.height) // cover
            case 4: scale = min(1, min(box.width / src.width, box.height / src.height))
            default: scale = min(box.width / src.width, box.height / src.height) // contain, none
            }
            let size = fit == 0 ? box.size
                                : CGSize(width: src.width * scale, height: src.height * scale)
            dest = CGRect(x: box.midX - size.width / 2,
                          y: box.midY - size.height / 2,
                          width: size.width, height: size.height)
        }
        if fit == 2 { // cover clips to the box
            NSBezierPath(rect: box).addClip()
        }
        // CGImage is y-down in page space like everything else here, so draw
        // flipped into the y-up AppKit context.
        let cgctx = NSGraphicsContext.current!.cgContext
        cgctx.saveGState()
        cgctx.translateBy(x: dest.midX, y: dest.midY)
        cgctx.scaleBy(x: 1, y: -1)
        cgctx.draw(image, in: CGRect(x: -dest.width / 2, y: -dest.height / 2,
                                     width: dest.width, height: dest.height))
        cgctx.restoreGState()
    }

    // MARK: Scrollbar (overlay)

    private func drawScrollbar() {
        drawContainerScrollbars()
        let maxOffset = maxScrollOffset()
        guard maxOffset > 0, bounds.height > 0, contentHeight > 0 else { return }

        let thumbFraction = min(1, bounds.height / contentHeight)
        let thumbHeight = max(28, bounds.height * thumbFraction)
        let scrollableSpan = bounds.height - thumbHeight
        let thumbY = scrollableSpan * (scrollOffset / maxOffset)

        let thumbWidth: CGFloat = 8
        let thumbRect = NSRect(x: bounds.width - thumbWidth - 3,
                               y: thumbY,
                               width: thumbWidth,
                               height: thumbHeight)
        let path = NSBezierPath(roundedRect: thumbRect, xRadius: thumbWidth / 2, yRadius: thumbWidth / 2)
        (isDarkAppearance ? NSColor.white.withAlphaComponent(0.25)
                          : NSColor.black.withAlphaComponent(0.22)).setFill()
        path.fill()
    }

    private func containerScrollbarRects(_ container: PageScrollContainer) -> (vertical: NSRect?, horizontal: NSRect?) {
        let visible = visibleRect(container)
        let verticalRange = maxContainerYOffset(container)
        let horizontalRange = maxContainerXOffset(container)
        var vertical: NSRect?
        var horizontal: NSRect?
        if container.h > 0, verticalRange > 0 || container.mode == 2 {
            let viewport = CGFloat(container.h)
            let content = max(1, CGFloat(container.contentHeight))
            let thumb = min(viewport, max(24, viewport * min(1, viewport / content)))
            let span = max(1, viewport - thumb)
            let offset = containerOffsets[container.node, default: 0]
            vertical = NSRect(x: visible.maxX - 11,
                              y: visible.minY + span * (offset / max(verticalRange, 1)),
                              width: 8, height: thumb)
        }
        if container.w > 0, horizontalRange > 0 || container.mode == 2 {
            let viewport = CGFloat(container.w)
            let content = max(1, CGFloat(container.contentWidth))
            let thumb = min(viewport, max(24, viewport * min(1, viewport / content)))
            let span = max(1, viewport - thumb)
            let offset = containerXOffsets[container.node, default: 0]
            horizontal = NSRect(x: visible.minX + span * (offset / max(horizontalRange, 1)),
                                y: visible.maxY - 11,
                                width: thumb, height: 8)
        }
        return (vertical, horizontal)
    }

    private func scrollbarHit(at point: NSPoint) -> (container: PageScrollContainer, vertical: Bool)? {
        guard let frame = pageFrame else { return nil }
        for container in frame.scrollContainers.reversed() {
            let rects = containerScrollbarRects(container)
            if let vertical = rects.vertical, vertical.contains(point) {
                return (container, true)
            }
            if let horizontal = rects.horizontal, horizontal.contains(point) {
                return (container, false)
            }
        }
        return nil
    }

    private func beginScrollbarDrag(_ hit: (container: PageScrollContainer, vertical: Bool), at point: NSPoint) {
        let offset = hit.vertical
            ? containerOffsets[hit.container.node, default: 0]
            : containerXOffsets[hit.container.node, default: 0]
        scrollbarDrag = (hit.container.node, hit.vertical,
                         hit.vertical ? point.y : point.x, origin: offset)
    }

    private func updateScrollbarDrag(to point: NSPoint) {
        guard let drag = scrollbarDrag,
              let container = pageFrame?.scrollContainers.first(where: { $0.node == drag.node }) else { return }
        let viewport = drag.vertical ? CGFloat(container.h) : CGFloat(container.w)
        let range = drag.vertical ? maxContainerYOffset(container) : maxContainerXOffset(container)
        let content = drag.vertical ? max(1, CGFloat(container.contentHeight)) : max(1, CGFloat(container.contentWidth))
        let thumb = min(viewport, max(24, viewport * min(1, viewport / content)))
        let span = max(1, viewport - thumb)
        let delta = (drag.vertical ? point.y - drag.start : point.x - drag.start)
        let offset = drag.origin + delta * range / span
        if drag.vertical {
            setContainerOffsets(x: containerXOffsets[container.node, default: 0], y: offset, for: container)
        } else {
            setContainerOffsets(x: offset, y: containerOffsets[container.node, default: 0], for: container)
        }
    }

    private func drawContainerScrollbars() {
        guard let frame = pageFrame else { return }
        for container in frame.scrollContainers {
            let rects = containerScrollbarRects(container)
            for rect in [rects.vertical, rects.horizontal].compactMap({ $0 }) {
                guard rect.intersects(bounds) else { continue }
                let path = NSBezierPath(roundedRect: rect, xRadius: 4, yRadius: 4)
                (isDarkAppearance ? NSColor.white.withAlphaComponent(0.28)
                                  : NSColor.black.withAlphaComponent(0.24)).setFill()
                path.fill()
            }
        }
    }

    private var isDarkAppearance: Bool {
        effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
    }

    // MARK: Accessibility (P2, H4)

    /// Cached AX elements for the current frame, parallel to
    /// `pageFrame.axNodes`. Rebuilt on frames and scrolls (frames ride in
    /// page space; AX wants view space).
    private var axElements: [NSAccessibilityElement] = []

    private var axLiveLabels: [String: (label: String, politeness: UInt8)] = [:]

    private func rebuildAxElements() {
        axElements = []
        guard let frame = pageFrame, !frame.axNodes.isEmpty else { return }
        axElements.reserveCapacity(frame.axNodes.count)
        for node in frame.axNodes {
            let semantic: PageAXElement
            if isPressableAxRole(node.role) {
                let pressable = PressableAXElement()
                pressable.enabled = !node.disabled
                pressable.onPress = { [weak self] in self?.pressAxNode(node) }
                semantic = pressable
            } else {
                semantic = PageAXElement()
            }
            let el: NSAccessibilityElement = semantic
            if node.role == 14 || node.role == 15 {
                semantic.onIncrement = { [weak self] in self?.onStepFocus?(1) }
                semantic.onDecrement = { [weak self] in self?.onStepFocus?(-1) }
            }
            semantic.headingLevel = node.headingLevel
            el.setAccessibilityRole(axRole(node.role))
            if node.role == 31 {
                el.setAccessibilitySubrole(NSAccessibility.Subrole(rawValue: "AXSwitch"))
            }
            el.setAccessibilityLabel(node.label.isEmpty ? nil : node.label)
            applyAxSemantics(to: el, node: node)
            let nodeBox = NSRect(x: CGFloat(node.x), y: CGFloat(node.y),
                                 width: CGFloat(node.w), height: CGFloat(node.h))
            let translation = containerTranslation(for: nodeBox)
            let localFrame = NSRect(x: nodeBox.minX - translation.x,
                                    y: nodeBox.minY - scrollOffset - translation.y,
                                    width: nodeBox.width,
                                    height: nodeBox.height)
            el.setAccessibilityFrame(NSAccessibility.screenRect(fromView: self, rect: localFrame))
            el.setAccessibilityFocused(node.id == frame.axFocusedId && !frame.axFocusedId.isEmpty)
            axElements.append(el)
        }
        for (index, node) in frame.axNodes.enumerated() {
            for child in node.children where child < axElements.count {
                axElements[child].setAccessibilityParent(axElements[index])
            }
        }
        for (index, parent) in frame.axParents.enumerated() {
            if parent == nil {
                axElements[index].setAccessibilityParent(self)
            }
        }
    }

    private func applyAxSemantics(to element: NSAccessibilityElement, node: AxNode) {
        element.setAccessibilityEnabled(!node.disabled)
        if let value = node.value {
            let numericRoles: Set<UInt8> = [14, 15, 16, 43]
            if numericRoles.contains(node.role), let number = Float(value), number.isFinite {
                element.setAccessibilityValue(NSNumber(value: number))
            } else {
                element.setAccessibilityValue(value)
            }
        }
        if let value = node.valueMin {
            element.setAccessibilityMinValue(NSNumber(value: value))
        }
        if let value = node.valueMax {
            element.setAccessibilityMaxValue(NSNumber(value: value))
        }
        if let expanded = node.expanded {
            element.setAccessibilityExpanded(expanded)
        }
        if let selected = node.selected {
            element.setAccessibilitySelected(selected)
        }
        switch node.checked {
        case .checked:
            element.setAccessibilityValue(NSNumber(value: 1))
        case .unchecked:
            element.setAccessibilityValue(NSNumber(value: 0))
        case .mixed:
            element.setAccessibilityValue(NSNumber(value: -1))
        case .unspecified:
            break
        }
        if node.role == 1, node.headingLevel > 0 {
            element.setAccessibilityValueDescription("Heading level \(node.headingLevel)")
        }
    }

    /// H4: wire roles an AX press can drive — links, buttons, checkboxes,
    /// text fields and combo boxes.
    private func isPressableAxRole(_ wire: UInt8) -> Bool {
        switch wire {
        case 2, 3, 4, 5, 6, 12, 13, 14, 15, 17, 31, 39: return true
        default: return false
        }
    }

    /// H4: AX press = a synthesized click at the node's center in page
    /// space. Zero-rect nodes (no geometry resolved) cannot be pressed.
    private func pressAxNode(_ node: AxNode) {
        guard node.w > 0, node.h > 0 else { return }
        let box = NSRect(x: CGFloat(node.x), y: CGFloat(node.y),
                         width: CGFloat(node.w), height: CGFloat(node.h))
        let translation = containerTranslation(for: box)
        onClick?(box.midX + translation.x, box.midY + translation.y)
    }

    /// H4: announce `aria-live` content changes through the AX announcement
    /// notification. Each frame's live-region labels diff against the
    /// previous frame's; every changed (or newly appeared non-empty) label
    /// posts one announcement. Polite and assertive share the engine's
    /// single live-region wire role, so all changes announce equally.
    private func announceLiveRegionChanges() {
        guard let frame = pageFrame else { return }
        var current: [String: (label: String, politeness: UInt8)] = [:]
        for node in frame.axNodes {
            let politeness: UInt8
            if node.livePoliteness == .off {
                guard node.role == 11 else { continue }
                politeness = AxLivePoliteness.polite.rawValue
            } else {
                politeness = node.livePoliteness.rawValue
            }
            current[node.id] = (node.label, politeness)
        }
        for (id, entry) in current where !entry.label.isEmpty {
            let previous = axLiveLabels[id]
            guard previous?.label != entry.label || previous?.politeness != entry.politeness else {
                continue
            }
            let priority = entry.politeness == AxLivePoliteness.assertive.rawValue ? 90 : 50
            NSAccessibility.post(element: self,
                                 notification: .announcementRequested,
                                 userInfo: [.announcement: entry.label,
                                            .priority: NSNumber(value: priority)])
        }
        axLiveLabels = current
    }

    /// Wire role id (kitae_core.h) → AppKit role (headings surface as
    /// static text — this SDK has no heading role; the wire id keeps the
    /// semantics for future hosts). Live regions (11, H4) surface as groups:
    /// AppKit has no live role — the announcements carry the semantics.
    private func axRole(_ wire: UInt8) -> NSAccessibility.Role {
        switch wire {
        case 1, 9, 40, 41: return .staticText
        case 2: return .link
        case 3, 31: return .button
        case 4: return .checkBox
        case 5: return .textField
        case 6: return .comboBox
        case 7: return .image
        case 8: return .table
        case 10, 11, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 38: return .group
        case 12: return .radioButton
        case 13: return .textArea
        case 14: return .slider
        case 15: return .incrementor
        case 16: return .progressIndicator
        case 17: return .colorWell
        case 18: return .list
        case 19, 34: return .row
        case 30: return .splitter
        case 39: return .disclosureTriangle
        case 32: return .radioGroup
        case 33: return .toolbar
        case 35, 36, 37: return .cell
        case 42: return .staticText
        case 43: return .scrollBar
        default: return .group
        }
    }

    override func isAccessibilityElement() -> Bool { false }

    override func accessibilityChildren() -> [Any]? {
        guard !axElements.isEmpty, let frame = pageFrame else { return nil }
        // Roots are the engine's Document node(s).
        let roots = axElements.enumerated().filter { frame.axParents[$0.offset] == nil }.map { $0.element }
        return roots.isEmpty ? nil : roots
    }
}

class PageAXElement: NSAccessibilityElement {
    var headingLevel: UInt8 = 0
    var onIncrement: (() -> Void)?
    var onDecrement: (() -> Void)?

    private var headingAttribute: NSAccessibility.Attribute {
        NSAccessibility.Attribute(rawValue: "AXHeadingLevel")
    }

    override func accessibilityAttributeNames() -> [NSAccessibility.Attribute] {
        var names = super.accessibilityAttributeNames()
        if headingLevel > 0 { names.append(headingAttribute) }
        return names
    }

    override func accessibilityAttributeValue(_ attribute: NSAccessibility.Attribute) -> Any? {
        if headingLevel > 0, attribute == headingAttribute {
            return NSNumber(value: headingLevel)
        }
        return super.accessibilityAttributeValue(attribute)
    }

    override func accessibilityIsAttributeSettable(_ attribute: NSAccessibility.Attribute) -> Bool {
        if headingLevel > 0, attribute == headingAttribute { return false }
        return super.accessibilityIsAttributeSettable(attribute)
    }

    func accessibilityIncrement() {
        onIncrement?()
    }

    func accessibilityDecrement() {
        onDecrement?()
    }
}

/// H4: a page AX element that carries an AXPress action. Pressing clicks
/// the node's center in page space — the engine resolves links, toggles
/// controls and places carets on the same path a mouse click uses.
final class PressableAXElement: PageAXElement {
    var enabled = true
    var onPress: (() -> Void)?

    override func accessibilityPerformPress() -> Bool {
        guard enabled, let onPress else { return false }
        onPress()
        return true
    }
}
