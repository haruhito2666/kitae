// LottieMenuButton.swift — animated menu button modeled after Menu-icon-kitae.lottie.
//
// In idle (closed) state: renders 3 crisp horizontal lines (hamburger icon).
// When clicked to open: morphs smoothly into an 'X' (close icon) and stays at 'X'
// for the entire time the slide drawer is open.
// When clicked to close: smoothly morphs back to 3 lines and settles in idle.
// Zero animation loops or unnecessary CPU consumption while idle or open.

import AppKit
import QuartzCore

final class LottieMenuButton: NSView {
    var action: () -> Void = {}

    private(set) var isOpen = false
    private var isHovered = false
    private var isPressed = false
    private var installedTrackingArea = false

    // Three bar layers representing the 3 layers in the Lottie file (Upper, Center, Bottom)
    private let upperBar = CAShapeLayer()
    private let centerBar = CAShapeLayer()
    private let bottomBar = CAShapeLayer()
    private let iconContainer = CALayer()

    private let buttonSide: CGFloat = 30
    private let iconSize: CGFloat = 16
    private let barWidth: CGFloat = 16
    private let barHeight: CGFloat = 2.0
    private let cornerRadius: CGFloat = 1.0

    override init(frame frameRect: NSRect) {
        super.init(frame: NSRect(x: 0, y: 0, width: buttonSide, height: buttonSide))
        wantsLayer = true
        layer?.cornerRadius = Theme.smallCornerRadius

        setupIconLayers()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    override var intrinsicContentSize: NSSize {
        NSSize(width: buttonSide, height: buttonSide)
    }

    override func layout() {
        super.layout()
        iconContainer.frame = CGRect(
            x: (bounds.width - iconSize) / 2,
            y: (bounds.height - iconSize) / 2,
            width: iconSize,
            height: iconSize
        )
    }

    // MARK: - Layer Setup

    private func setupIconLayers() {
        guard let rootLayer = layer else { return }

        iconContainer.frame = CGRect(
            x: (bounds.width > 0 ? bounds.width - iconSize : buttonSide - iconSize) / 2,
            y: (bounds.height > 0 ? bounds.height - iconSize : buttonSide - iconSize) / 2,
            width: iconSize,
            height: iconSize
        )
        rootLayer.addSublayer(iconContainer)

        let barPath = CGPath(
            roundedRect: CGRect(x: -barWidth / 2, y: -barHeight / 2, width: barWidth, height: barHeight),
            cornerWidth: cornerRadius,
            cornerHeight: cornerRadius,
            transform: nil
        )

        for bar in [upperBar, centerBar, bottomBar] {
            bar.path = barPath
            bar.anchorPoint = CGPoint(x: 0.5, y: 0.5)
            iconContainer.addSublayer(bar)
        }

        updateBarColors()
        applyState(open: false, animated: false)
    }

    private func updateBarColors() {
        let tint = isHovered || isPressed ? Theme.chromeText : Theme.chromeMutedText
        let cgColor = Theme.cgColor(of: tint, for: effectiveAppearance)

        upperBar.fillColor = cgColor
        centerBar.fillColor = cgColor
        bottomBar.fillColor = cgColor
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateBarColors()
        needsDisplay = true
    }

    // MARK: - State & Animations

    func setOpen(_ open: Bool, animated: Bool) {
        guard isOpen != open else { return }
        isOpen = open
        applyState(open: open, animated: animated)
    }

    private func applyState(open: Bool, animated: Bool) {
        let centerX = iconSize / 2
        let centerY = iconSize / 2
        let spacing: CGFloat = 5.2

        // Target geometry:
        // Closed: Upper (y=centerY-spacing, rot=0), Center (y=centerY, rot=0), Bottom (y=centerY+spacing, rot=0)
        // Open: Upper (y=centerY, rot=-45°), Center (y=centerY, rot=+45°), Bottom (y=centerY, rot=-45°)
        let upperTargetY = open ? centerY : centerY - spacing
        let bottomTargetY = open ? centerY : centerY + spacing

        let upperRot: CGFloat = open ? -.pi / 4 : 0
        let centerRot: CGFloat = open ? .pi / 4 : 0
        let bottomRot: CGFloat = open ? -.pi / 4 : 0

        if animated && Theme.motionEnabled {
            let duration: TimeInterval = 0.26
            let timing = CAMediaTimingFunction(controlPoints: 0.33, 0.0, 0.67, 1.0)

            animateBar(upperBar, targetPos: CGPoint(x: centerX, y: upperTargetY), targetRot: upperRot, duration: duration, timing: timing)
            animateBar(centerBar, targetPos: CGPoint(x: centerX, y: centerY), targetRot: centerRot, duration: duration, timing: timing)
            animateBar(bottomBar, targetPos: CGPoint(x: centerX, y: bottomTargetY), targetRot: bottomRot, duration: duration, timing: timing)
        } else {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            upperBar.position = CGPoint(x: centerX, y: upperTargetY)
            upperBar.transform = CATransform3DMakeRotation(upperRot, 0, 0, 1)

            centerBar.position = CGPoint(x: centerX, y: centerY)
            centerBar.transform = CATransform3DMakeRotation(centerRot, 0, 0, 1)

            bottomBar.position = CGPoint(x: centerX, y: bottomTargetY)
            bottomBar.transform = CATransform3DMakeRotation(bottomRot, 0, 0, 1)
            CATransaction.commit()
        }
    }

    private func animateBar(_ bar: CAShapeLayer, targetPos: CGPoint, targetRot: CGFloat, duration: TimeInterval, timing: CAMediaTimingFunction) {
        let posAnim = CABasicAnimation(keyPath: "position")
        posAnim.fromValue = bar.presentation()?.position ?? bar.position
        posAnim.toValue = targetPos

        let rotAnim = CABasicAnimation(keyPath: "transform.rotation.z")
        let currentRot = (bar.presentation()?.value(forKeyPath: "transform.rotation.z") as? CGFloat)
            ?? (bar.value(forKeyPath: "transform.rotation.z") as? CGFloat) ?? 0
        rotAnim.fromValue = currentRot
        rotAnim.toValue = targetRot

        let group = CAAnimationGroup()
        group.animations = [posAnim, rotAnim]
        group.duration = duration
        group.timingFunction = timing
        group.fillMode = .forwards
        group.isRemovedOnCompletion = false

        bar.position = targetPos
        bar.transform = CATransform3DMakeRotation(targetRot, 0, 0, 1)
        bar.add(group, forKey: "lottieMorph")
    }

    // MARK: - Drawing & Interactions

    override func draw(_ dirtyRect: NSRect) {
        if isPressed || isHovered {
            (isPressed ? Theme.pressFill : Theme.hoverFill).setFill()
            NSBezierPath(roundedRect: bounds, xRadius: Theme.smallCornerRadius,
                         yRadius: Theme.smallCornerRadius).fill()
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
        updateBarColors()
        Theme.animate { needsDisplay = true }
    }

    override func mouseExited(with event: NSEvent) {
        isHovered = false
        updateBarColors()
        Theme.animate { needsDisplay = true }
    }

    override func mouseDown(with event: NSEvent) {
        isPressed = true
        updateBarColors()
        Theme.animate { needsDisplay = true }
    }

    override func mouseUp(with event: NSEvent) {
        let wasInside = isPressed
        isPressed = false
        updateBarColors()
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
