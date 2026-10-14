// Icons.swift — small shared view primitives: monograms, tinted glyph
// buttons with springy press states, checkbox and stepper controls, and the
// hairline/paint helpers the chrome and settings reuse.
import AppKit

// MARK: - Monogram badge

/// A rounded square with the site's initial, tinted by a stable per-title
/// hue. Every tab list, the palette and settings reuse this.
final class MonogramView: NSView {
    private let label = NSTextField(labelWithString: "")
    private let iconImageView = NSImageView()
    private var tintColor: NSColor = Theme.accent
    private let radius: CGFloat
    private var isSpecialSite = false

    init(radius: CGFloat = 7) {
        self.radius = radius
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = radius
        layer?.masksToBounds = true

        label.font = NSFont.systemFont(ofSize: max(8, radius * 0.64), weight: .bold)
        label.alignment = .center
        addSubview(label)

        iconImageView.imageScaling = .scaleProportionallyUpOrDown
        iconImageView.isHidden = true
        addSubview(iconImageView)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func configure(title: String) {
        let lower = title.lowercased()
        if lower.contains("youtube") {
            isSpecialSite = true
            label.isHidden = true
            iconImageView.isHidden = false
            tintColor = NSColor(red: 1.0, green: 0.12, blue: 0.12, alpha: 1.0)
            if let playImg = NSImage(systemSymbolName: "play.fill", accessibilityDescription: nil)?
                .withSymbolConfiguration(.init(pointSize: max(7, radius * 0.70), weight: .bold)) {
                let tinted = NSImage(size: playImg.size, flipped: false) { rect in
                    playImg.draw(in: rect)
                    NSColor.white.setFill()
                    rect.fill(using: .sourceAtop)
                    return true
                }
                iconImageView.image = tinted
            }
        } else {
            isSpecialSite = false
            label.isHidden = false
            iconImageView.isHidden = true
            let letter = title.first.map { String($0).uppercased() } ?? "•"
            label.stringValue = letter
            tintColor = Theme.tint(for: title)
        }
        refresh()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if isSpecialSite {
            layer?.backgroundColor = tintColor.cgColor
            layer?.cornerRadius = 5
        } else {
            layer?.backgroundColor = Theme.cgColor(of: tintColor.withAlphaComponent(0.20),
                                                  for: effectiveAppearance)
            layer?.cornerRadius = radius
            label.textColor = tintColor
        }
        CATransaction.commit()
    }

    override func layout() {
        super.layout()
        if isSpecialSite {
            layer?.cornerRadius = 5
            iconImageView.frame = bounds.insetBy(dx: 2.5, dy: 2.5)
        } else {
            layer?.cornerRadius = radius
            label.frame = label.centeredTextFrame(x: 0, width: bounds.width,
                                                  inHeight: bounds.height)
        }
    }
}

// MARK: - Glyph button (hover lift + press bounce)

/// A borderless SF Symbol button: hover fill, press scale, fire bounce.
final class GlyphButton: NSView {
    private let symbolName: String
    private let customImage: NSImage?
    var action: () -> Void
    private var isHovered = false
    private var isPressed = false
    private var installedTrackingArea = false
    private let glyphSize: CGFloat
    private let pill: Bool

    var isEnabled = true { didSet {
        setAccessibilityEnabled(isEnabled)
        needsDisplay = true
    } }
    var isDestructive = false { didSet { needsDisplay = true } }

    init(symbolName: String, glyphSize: CGFloat = 13, pill: Bool = true,
         action: @escaping () -> Void = {}) {
        self.symbolName = symbolName
        self.customImage = nil
        self.action = action
        self.glyphSize = glyphSize
        self.pill = pill
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(symbolName)
        setAccessibilityEnabled(true)
        wantsLayer = true
        layer?.cornerRadius = pill ? Theme.smallCornerRadius : 0
    }

    init(image: NSImage, glyphSize: CGFloat = 16, pill: Bool = true,
         action: @escaping () -> Void = {}) {
        self.symbolName = ""
        self.customImage = image
        self.action = action
        self.glyphSize = glyphSize
        self.pill = pill
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(image.accessibilityDescription ?? "Button")
        setAccessibilityEnabled(true)
        wantsLayer = true
        layer?.cornerRadius = pill ? Theme.smallCornerRadius : 0
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override var acceptsFirstResponder: Bool { true }

    override func accessibilityPerformPress() -> Bool {
        guard isEnabled else { return false }
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

    override func draw(_ dirtyRect: NSRect) {
        // Fill only on hover/press; the glyph always draws (dimmed when
        // disabled) so idle and disabled buttons stay visible.
        if pill, isPressed || (isHovered && isEnabled) {
            (isPressed ? Theme.pressFill : Theme.hoverFill).setFill()
            NSBezierPath(roundedRect: bounds, xRadius: Theme.smallCornerRadius,
                         yRadius: Theme.smallCornerRadius).fill()
        }

        let base: NSImage?
        if let customImage {
            base = customImage
        } else {
            base = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil)?
                .withSymbolConfiguration(.init(pointSize: glyphSize, weight: .medium))
        }
        guard let base else { return }

        // For custom (SVG) images: render at natural size scaled to glyphSize,
        // preserving aspect ratio so stroked paths keep their visual weight.
        // For SF Symbols: the symbol config already bakes in the right size.
        let renderSize: CGSize
        if customImage != nil {
            // Scale natural image proportionally so the larger dimension == glyphSize
            let nat = base.size
            let scale = glyphSize / max(nat.width, nat.height)
            renderSize = CGSize(width: nat.width * scale, height: nat.height * scale)
        } else {
            renderSize = CGSize(width: glyphSize, height: glyphSize)
        }

        let img = NSImage(size: renderSize, flipped: false) { rect in
            let tint: NSColor
            if !self.isEnabled {
                tint = Theme.buttonGlyphDisabled
            } else if self.isDestructive {
                tint = Theme.accent
            } else {
                tint = self.isHovered || self.isPressed ? Theme.chromeText : Theme.chromeMutedText
            }
            base.draw(in: rect)
            tint.setFill()
            rect.fill(using: .sourceAtop)
            return true
        }
        let origin = NSPoint(x: (bounds.width - renderSize.width) / 2,
                             y: (bounds.height - renderSize.height) / 2)
        img.draw(at: origin, from: .zero, operation: .sourceOver, fraction: 1)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if !installedTrackingArea {
            installedTrackingArea = true
            addTrackingArea(NSTrackingArea(rect: bounds,
                                           options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                           owner: self))
        }
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        Theme.animate { needsDisplay = true }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        Theme.animate { needsDisplay = true }
    }

    override func mouseDown(with event: NSEvent) {
        guard isEnabled else { return }
        isPressed = true
        Theme.animate { needsDisplay = true }
        pressBounce(down: true)
    }

    override func mouseUp(with event: NSEvent) {
        let wasInside = isPressed
        isPressed = false
        needsDisplay = true
        pressBounce(down: false)
        if wasInside, bounds.contains(convert(event.locationInWindow, from: nil)) {
            Theme.bounce(self)
            action()
        }
    }

    /// Springy scale while pressed (layer-backed); a no-op when the view has
    /// no layer or motion is off.
    private func pressBounce(down: Bool) {
        guard Theme.motionEnabled, let layer else { return }
        let to = down ? Theme.pressScale : 1.0
        let anim = CASpringAnimation(keyPath: "transform.scale")
        anim.toValue = to
        anim.damping = 14
        anim.initialVelocity = 4
        anim.mass = 0.7
        anim.stiffness = 220
        anim.duration = 0.3
        layer.add(anim, forKey: "press")
        layer.setValue(to, forKeyPath: "transform.scale")
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Pill button (labeled, like "Share")

/// A small rounded label button — the toolbar's trailing pills and the
/// settings pane's actions use it.
final class PillButton: NSView {
    private let label = NSTextField(labelWithString: "")
    private let symbolName: String?
    var action: () -> Void
    var isPressed = false
    private var isHovered = false
    private var installedTrackingArea = false

    init(title: String, symbolName: String? = nil, action: @escaping () -> Void = {}) {
        self.symbolName = symbolName
        self.action = action
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(title)
        setAccessibilityEnabled(true)
        wantsLayer = true
        layer?.cornerRadius = Theme.pillHeight / 2
        label.stringValue = title
        label.font = Theme.fontButtonMedium
        addSubview(label)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var intrinsicContentSize: NSSize {
        let iconSpace: CGFloat = symbolName == nil ? 0 : 15
        let textWidth = label.intrinsicContentSize.width
        return NSSize(width: textWidth + iconSpace + 22, height: Theme.pillHeight)
    }

    override var isFlipped: Bool { true }

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
        let iconSpace: CGFloat = symbolName == nil ? 0 : 15
        let textWidth = label.intrinsicContentSize.width
        let width = textWidth + iconSpace + 22
        let size = NSSize(width: width, height: Theme.pillHeight)
        if frame.size != size { setFrameSize(size) }
        label.frame = label.centeredTextFrame(x: 11 + iconSpace, width: textWidth,
                                              inHeight: Theme.pillHeight)
    }

    override func draw(_ dirtyRect: NSRect) {
        let fill = isPressed ? Theme.pressFill : Theme.hoverFill
        (isPressed || isHovered ? fill : Theme.addressFieldBackground).setFill()
        NSBezierPath(roundedRect: bounds, xRadius: bounds.height / 2,
                     yRadius: bounds.height / 2).fill()
        Theme.addressFieldBorder.setStroke()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5),
                     xRadius: bounds.height / 2, yRadius: bounds.height / 2).stroke()
        if let symbolName,
           let icon = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 10, weight: .medium)) {
            let img = NSImage(size: NSSize(width: 11, height: 11), flipped: false) { rect in
                icon.draw(in: rect)
                Theme.chromeText.setFill()
                rect.fill(using: .sourceAtop)
                return true
            }
            img.draw(at: NSPoint(x: 11, y: (Theme.pillHeight - 11) / 2),
                     from: .zero, operation: .sourceOver, fraction: 1)
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if !installedTrackingArea {
            installedTrackingArea = true
            addTrackingArea(NSTrackingArea(rect: bounds,
                                           options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                           owner: self))
        }
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        Theme.animate { needsDisplay = true }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
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

// MARK: - Checkbox

/// A custom-drawn checkmark with an accent fill and a little pop when
/// toggled — used in the Settings panes.
final class Checkbox: NSControl {
    var onChange: ((Bool) -> Void)?
    private var checked: Bool = false
    private var isHovered = false
    private var installedTrackingArea = false

    var isChecked: Bool {
        get { checked }
        set { setChecked(newValue, animated: false) }
    }

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: 16, height: 16))
        setAccessibilityElement(true)
        setAccessibilityRole(.checkBox)
        setAccessibilityValue(NSNumber(value: false))
        wantsLayer = true
        layer?.cornerRadius = 4.5
        layer?.masksToBounds = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var acceptsFirstResponder: Bool { true }

    override func accessibilityPerformPress() -> Bool {
        setChecked(!checked, animated: true)
        onChange?(checked)
        return true
    }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 76 || event.keyCode == 49 {
            _ = accessibilityPerformPress()
        } else {
            super.keyDown(with: event)
        }
    }

    func setChecked(_ value: Bool, animated: Bool) {
        checked = value
        setAccessibilityValue(NSNumber(value: value))
        if animated {
            Theme.animate { self.refresh() }
            if Theme.motionEnabled, let layer = layer {
                let pop = CAKeyframeAnimation(keyPath: "transform.scale")
                pop.values = checked ? [1, 1.28, 1] : [1, 0.85, 1]
                pop.duration = 0.24
                layer.add(pop, forKey: "pop")
            }
        } else {
            refresh()
        }
    }

    private func refresh() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(
            of: checked ? Theme.accent : Theme.addressFieldBackground,
            for: effectiveAppearance)
        CATransaction.commit()
        needsDisplay = true
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    override func draw(_ dirtyRect: NSRect) {
        if checked {
            let check = "✓"
            let attrs: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: 11, weight: .bold),
                .foregroundColor: NSColor.white,
            ]
            let size = (check as NSString).size(withAttributes: attrs)
            (check as NSString).draw(at: NSPoint(x: (bounds.width - size.width) / 2,
                                                 y: (bounds.height - size.height) / 2),
                                     withAttributes: attrs)
        } else if isHovered {
            Theme.hoverFill.setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 4.5, yRadius: 4.5).fill()
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if !installedTrackingArea {
            installedTrackingArea = true
            addTrackingArea(NSTrackingArea(rect: bounds,
                                           options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                           owner: self))
        }
    }

    override func mouseEntered(with event: NSEvent) {
        isHovered = true
        needsDisplay = true
    }
    override func mouseExited(with event: NSEvent) {
        isHovered = false
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        setChecked(!checked, animated: true)
        onChange?(checked)
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Segmented stepper (the "12 hours" control)

/// A compact popup-style stepper for enumeration choices in Settings.
final class MiniStepper: NSControl {
    private let options: [String]
    private(set) var selectedIndex: Int
    var onChange: ((Int) -> Void)?
    private let label = NSTextField(labelWithString: "")
    private var hoverTracking = false
    private var openMenu: NSMenu?

    init(options: [String], selectedIndex: Int) {
        self.options = options
        self.selectedIndex = selectedIndex
        super.init(frame: .zero)
        wantsLayer = true
        label.font = Theme.fontSettingsRow
        label.textColor = Theme.chromeMutedText
        addSubview(label)
        refresh()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func refresh() {
        label.stringValue = options[selectedIndex]
        needsDisplay = true
    }

    override func layout() {
        super.layout()
        let textWidth = label.intrinsicContentSize.width
        setFrameSize(NSSize(width: textWidth + 34, height: 22))
        label.frame = NSRect(x: 10, y: 0, width: textWidth, height: 22)
        layer?.cornerRadius = 6
    }

    override func draw(_ dirtyRect: NSRect) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer?.backgroundColor = Theme.cgColor(of: Theme.addressFieldBackground,
                                              for: effectiveAppearance)
        CATransaction.commit()
        Theme.addressFieldBorder.setStroke()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5),
                     xRadius: 6, yRadius: 6).stroke()
        // Up/down chevrons on the right.
        Theme.chromeMutedText.setStroke()
        let cx = bounds.maxX - 12
        let cy = bounds.midY
        for (dy, dir) in [(-2.5, 1.0), (2.5, -1.0)] {
            let p = NSBezierPath()
            p.move(to: NSPoint(x: cx - 3, y: cy + dy + dir * 1.5))
            p.line(to: NSPoint(x: cx, y: cy + dy - dir * 1.5))
            p.line(to: NSPoint(x: cx + 3, y: cy + dy + dir * 1.5))
            p.lineWidth = 1.2
            p.stroke()
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        let menu = NSMenu()
        for (index, option) in options.enumerated() {
            let item = NSMenuItem(title: option, action: #selector(choose(_:)), keyEquivalent: "")
            item.target = self
            item.tag = index
            item.state = index == selectedIndex ? .on : .off
            menu.addItem(item)
        }
        menu.popUp(positioning: menu.item(withTag: selectedIndex), at: NSPoint(x: 0, y: bounds.maxY + 4),
                   in: self)
    }

    @objc private func choose(_ sender: NSMenuItem) {
        selectedIndex = sender.tag
        refresh()
        onChange?(selectedIndex)
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Paint helpers

extension NSTextField {
    /// A frame that vertically centers this label's real rendered height
    /// inside `containerHeight` — the fix for text sitting off-center in
    /// fixed-height rows, cells and pills.
    func centeredTextFrame(x: CGFloat, width: CGFloat, inHeight containerHeight: CGFloat) -> NSRect {
        let textHeight = ceil(intrinsicContentSize.height)
        return NSRect(x: x, y: (containerHeight - textHeight) / 2,
                      width: width, height: textHeight)
    }
}

final class HairlineView: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        Theme.hairline.setFill()
        bounds.fill()
    }
}

/// The window backdrop: a soft vertical gradient everything floats on.
final class BackdropView: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        ctx.saveGState()
        ctx.setFillColor(Theme.cgColor(of: Theme.backdropTop, for: effectiveAppearance))
        ctx.fill(bounds)
        let colors = [
            Theme.cgColor(of: Theme.backdropTop, for: effectiveAppearance),
            Theme.cgColor(of: Theme.backdropBottom, for: effectiveAppearance),
        ] as CFArray
        let space = CGColorSpaceCreateDeviceRGB()
        if let gradient = CGGradient(colorsSpace: space, colors: colors, locations: [0, 1]) {
            ctx.drawLinearGradient(gradient,
                                   start: CGPoint(x: 0, y: 0),
                                   end: CGPoint(x: 0, y: bounds.height),
                                   options: [])
        }
        ctx.restoreGState()
    }
}

// MARK: - Tabler Link Icon

extension NSImage {
    static let linkIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="black">
          <path stroke="none" d="M0 0h24v24H0z" fill="none" />
          <path d="M15.707 8.293a1 1 0 0 1 0 1.414l-6 6a1 1 0 1 1 -1.414 -1.414l6 -6a1 1 0 0 1 1.414 0" />
          <path d="M19.242 4.757c2.343 2.344 2.342 6.143 -.052 8.534l-.534 .464a1 1 0 1 1 -1.312 -1.51l.483 -.416a4 4 0 0 0 0 -5.657c-1.562 -1.563 -4.095 -1.563 -5.607 -.054l-.463 .536a1 1 0 1 1 -1.514 -1.308l.513 -.59a6 6 0 0 1 8.486 .001" />
          <path d="M6.75 10.338a1 1 0 0 1 -.088 1.411l-.483 .425a3.97 3.97 0 0 0 0 5.649a4.064 4.064 0 0 0 5.678 .038l.34 -.458a1 1 0 1 1 1.606 1.194l-.397 .534l-.1 .114a6.07 6.07 0 0 1 -8.533 0a5.97 5.97 0 0 1 -1.773 -4.247c0 -1.595 .638 -3.124 1.814 -4.284l.524 -.463a1 1 0 0 1 1.411 .087" />
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    static let adjustmentsIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="black">
          <path stroke="none" d="M0 0h24v24H0z" fill="none" />
          <path d="M17 15c1.306 0 2.418 .835 2.83 2h.17a1 1 0 0 1 0 2h-.171a3.001 3.001 0 0 1 -5.658 0h-10.171a1 1 0 0 1 0 -2h10.173c.41 -1.165 1.52 -2 2.827 -2m-9 -6c1.306 0 2.418 .835 2.83 2h9.17a1 1 0 0 1 0 2h-9.171a3.001 3.001 0 0 1 -5.658 0h-1.171a1 1 0 0 1 0 -2h1.173c.41 -1.165 1.52 -2 2.827 -2m6 -6c1.306 0 2.418 .835 2.83 2h3.17a1 1 0 0 1 0 2h-3.171a3.001 3.001 0 0 1 -5.658 0h-7.171a1 1 0 1 1 0 -2h7.173c.41 -1.165 1.52 -2 2.827 -2" />
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    static let tablerLockIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="black">
          <path stroke="none" d="M0 0h24v24H0z" fill="none" />
          <path d="M12 2a5 5 0 0 1 5 5v3a3 3 0 0 1 3 3v6a3 3 0 0 1 -3 3h-10a3 3 0 0 1 -3 -3v-6a3 3 0 0 1 3 -3v-3a5 5 0 0 1 5 -5m0 12a2 2 0 0 0 -1.995 1.85l-.005 .15a2 2 0 1 0 2 -2m0 -10a3 3 0 0 0 -3 3v3h6v-3a3 3 0 0 0 -3 -3" />
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    static let tablerCookieIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="black">
          <path stroke="none" d="M0 0h24v24H0z" fill="none" />
          <path d="M13.53 2.552l2.667 1.104a1 1 0 0 1 .414 1.53a3 3 0 0 0 3.492 4.604a1 1 0 0 1 1.296 .557l.049 .122a4 4 0 0 1 0 3.062l-.079 .151c-.467 .74 -.785 1.314 -.945 1.7c-.166 .4 -.373 1.097 -.613 2.073l-.047 .144a4 4 0 0 1 -2.166 2.164l-.139 .046c-1.006 .253 -1.705 .461 -2.076 .615c-.412 .17 -.982 .486 -1.696 .942l-.156 .082a4 4 0 0 1 -3.062 0l-.148 -.077c-.759 -.475 -1.333 -.793 -1.704 -.947c-.413 -.171 -1.109 -.378 -2.07 -.612l-.146 -.048a4 4 0 0 1 -2.164 -2.166l-.046 -.138c-.254 -1.009 -.463 -1.709 -.615 -2.078q -.256 -.621 -.942 -1.695l-.082 -.156a4 4 0 0 1 0 -3.062l.084 -.16c.447 -.692 .761 -1.262 .94 -1.692c.147 -.355 .356 -1.057 .615 -2.078l.045 -.138a4 4 0 0 1 2.166 -2.164l.141 -.047c.988 -.245 1.686 -.453 2.074 -.614c.395 -.164 .967 -.48 1.7 -.944l.152 -.08a4 4 0 0 1 3.062 0m-1.531 13.448a1 1 0 0 0 -1 1v.01a1 1 0 0 0 2 0v-.01a1 1 0 0 0 -1 -1m4 -3a1 1 0 0 0 -1 1v.01a1 1 0 0 0 2 0v-.01a1 1 0 0 0 -1 -1m-8 -1a1 1 0 0 0 -1 1v.01a1 1 0 0 0 2 0v-.01a1 1 0 0 0 -1 -1m4 -1a1 1 0 0 0 -1 1v.01a1 1 0 0 0 2 0v-.01a1 1 0 0 0 -1 -1m-1 -4c-.552 0 -1 .448 -1 1.01a1 1 0 1 0 2 -.01a1 1 0 0 0 -1 -1" />
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    static let tablerSettingsIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="black">
          <path stroke="none" d="M0 0h24v24H0z" fill="none" />
          <path d="M14.647 4.081a.724 .724 0 0 0 1.08 .448c2.439 -1.485 5.23 1.305 3.745 3.744a.724 .724 0 0 0 .447 1.08c2.775 .673 2.775 4.62 0 5.294a.724 .724 0 0 0 -.448 1.08c1.485 2.439 -1.305 5.23 -3.744 3.745a.724 .724 0 0 0 -1.08 .447c-.673 2.775 -4.62 2.775 -5.294 0a.724 .724 0 0 0 -1.08 -.448c-2.439 1.485 -5.23 -1.305 -3.745 -3.744a.724 .724 0 0 0 -.447 -1.08c-2.775 -.673 -2.775 -4.62 0 -5.294a.724 .724 0 0 0 .448 -1.08c-1.485 -2.439 1.305 -5.23 3.744 -3.745a.722 .722 0 0 0 1.08 -.447c.673 -2.775 4.62 -2.775 5.294 0zm-2.647 4.919a3 3 0 1 0 0 6a3 3 0 0 0 0 -6" />
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    /// Sidebar is currently collapsed — clicking will expand it.
    static let sidebarExpandIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path stroke="none" d="M0 0h24v24H0z" fill="none"/>
          <path d="M4 6a2 2 0 0 1 2 -2h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2l0 -12"/>
          <path d="M9 4v16"/>
          <path d="M14 10l2 2l-2 2"/>
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()

    /// Sidebar is currently visible — clicking will collapse it.
    static let sidebarCollapseIcon: NSImage = {
        let svg = """
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path stroke="none" d="M0 0h24v24H0z" fill="none"/>
          <path d="M4 6a2 2 0 0 1 2 -2h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2l0 -12"/>
          <path d="M9 4v16"/>
          <path d="M15 10l-2 2l2 2"/>
        </svg>
        """
        guard let data = svg.data(using: .utf8), let img = NSImage(data: data) else {
            return NSImage()
        }
        img.isTemplate = true
        return img
    }()
}

