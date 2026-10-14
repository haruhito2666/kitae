import AppKit
import QuartzCore

// NativeReloadButton — reload button driven by Reload-Page.lottie.
// Sized like the other toolbar icons (16 pt glyph in a 30×30 hit target) and
// tinted chromeMutedText at rest → chromeText on hover/press, matching
// LottieMenuButton. The Lottie plays once per reload and holds its final
// frame until the next reload (no looping).
//
// P-3 (SECURITY_AUDIT.md): native Core Animation rendering, no WKWebView,
// no JS engine, no WebContent process.
//
// Drop-in replacement for the previous CAShapeLayer spinner — same
// `var action: () -> Void` interface, same `setLoading(_:)` call.
final class LottieReloadButton: NSView {

    var action: () -> Void = {}

    private let buttonSide: CGFloat = 30
    private let iconSize: CGFloat = 16
    private var icon: LottieView?
    private var isHovered = false
    private var isPressed = false
    private var trackingArea: NSTrackingArea?

    override init(frame: NSRect) {
        super.init(frame: NSRect(x: 0, y: 0, width: buttonSide, height: buttonSide))
        wantsLayer = true
        layer?.cornerRadius = Theme.smallCornerRadius
        if let doc = LottieDocument.named("Reload-Page") {
            let view = LottieView(document: doc, tint: idleTint)
            addSubview(view)
            icon = view
        }
        updateTint()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override var intrinsicContentSize: NSSize {
        NSSize(width: buttonSide, height: buttonSide)
    }

    private var idleTint: NSColor { Theme.chromeMutedText }
    private var activeTint: NSColor { Theme.chromeText }

    private func updateTint() {
        icon?.setTint(isHovered || isPressed ? activeTint : idleTint)
    }

    override func layout() {
        super.layout()
        icon?.frame = CGRect(
            x: (bounds.width - iconSize) / 2,
            y: (bounds.height - iconSize) / 2,
            width: iconSize,
            height: iconSize
        )
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateTint()
    }

    // MARK: - Public API

    /// Play the reload Lottie once and hold its final frame. Setting loading
    /// off is a no-op: the icon persists until the next reload plays it again.
    func setLoading(_ loading: Bool) {
        guard loading else { return }
        icon?.playOnce()
    }

    // MARK: - Mouse handling

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let ta = trackingArea { removeTrackingArea(ta) }
        let ta = NSTrackingArea(rect: bounds,
                                options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                owner: self, userInfo: nil)
        addTrackingArea(ta)
        trackingArea = ta
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        updateTint()
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        isPressed = false
        updateTint()
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        isPressed = true
        updateTint()
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        let wasPressed = isPressed
        isPressed = false
        updateTint()
        needsDisplay = true
        let inside = bounds.contains(convert(event.locationInWindow, from: nil))
        if wasPressed && inside {
            action()
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        if isPressed || isHovered {
            (isPressed ? Theme.pressFill : Theme.hoverFill).setFill()
            NSBezierPath(roundedRect: bounds,
                         xRadius: Theme.smallCornerRadius,
                         yRadius: Theme.smallCornerRadius).fill()
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
