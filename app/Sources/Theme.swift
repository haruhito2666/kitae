// Theme.swift — the Kitae design system: one place for every color, font,
// spacing and animation constant the chrome uses. Appearance follows the
// system (light/dark) unless the user pins one in Settings, and all motion
// collapses to zero under Reduce Motion.
import AppKit

enum Theme {

    // MARK: Palette

    /// Dynamic provider colors: both light and dark variants are declared
    /// here; macOS picks per current appearance and updates live.
    static func dynamic(_ light: NSColor, _ dark: NSColor) -> NSColor {
        NSColor(name: nil) { appearance in
            let isDark = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            return isDark ? dark : light
        }
    }

    /// The page canvas always paints on an opaque paper color; the engine's
    /// page_background (when opaque) overrides this per frame. These are the
    /// floating content card's paper.
    static let canvasLight = NSColor(red: 0.988, green: 0.988, blue: 0.980, alpha: 1)
    static let canvasDark = NSColor(red: 0.125, green: 0.125, blue: 0.153, alpha: 1)

    /// Window backdrop the chrome and the floating card sit on (a soft
    /// vertical gradient between these two).
    static let backdropTop = dynamic(
        NSColor(red: 0.949, green: 0.945, blue: 0.933, alpha: 1),
        NSColor(red: 0.094, green: 0.090, blue: 0.110, alpha: 1))
    static let backdropBottom = dynamic(
        NSColor(red: 0.918, green: 0.913, blue: 0.898, alpha: 1),
        NSColor(red: 0.071, green: 0.067, blue: 0.086, alpha: 1))

    static let sidebarBackground = dynamic(
        NSColor.clear,
        NSColor.clear)

    static let cardBackground = dynamic(
        NSColor(red: 0.988, green: 0.988, blue: 0.980, alpha: 1),
        NSColor(red: 0.125, green: 0.125, blue: 0.153, alpha: 1))
    static let cardBorder = dynamic(
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.55),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.07))

    /// Periwinkle accent — soft enough for fills, clear enough for focus.
    static let accent = dynamic(
        NSColor(red: 0.357, green: 0.404, blue: 0.847, alpha: 1),
        NSColor(red: 0.494, green: 0.537, blue: 0.953, alpha: 1))

    static let chromeText = dynamic(
        NSColor(red: 0.13, green: 0.13, blue: 0.15, alpha: 1),
        NSColor(red: 0.93, green: 0.93, blue: 0.95, alpha: 1))
    static let chromeMutedText = dynamic(
        NSColor(red: 0.42, green: 0.42, blue: 0.46, alpha: 1),
        NSColor(red: 0.60, green: 0.60, blue: 0.66, alpha: 1))

    static let addressFieldBackground = dynamic(
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.70),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.08))
    static let addressFieldBorder = dynamic(
        NSColor(red: 0.78, green: 0.77, blue: 0.75, alpha: 1),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.09))

    static let hoverFill = dynamic(
        NSColor(red: 0, green: 0, blue: 0, alpha: 0.065),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.075))
    static let pressFill = dynamic(
        NSColor(red: 0, green: 0, blue: 0, alpha: 0.11),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.12))
    static let tabSelected = dynamic(
        NSColor(white: 1.0, alpha: 1.0),
        NSColor(white: 0.22, alpha: 1.0))
    static let selectionFill = dynamic(
        NSColor(red: 0.357, green: 0.404, blue: 0.847, alpha: 0.14),
        NSColor(red: 0.494, green: 0.537, blue: 0.953, alpha: 0.26))

    static let hairline = dynamic(
        NSColor(red: 0, green: 0, blue: 0, alpha: 0.10),
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.08))

    static let loadingGradient: [NSColor] = [
        accent.withAlphaComponent(0.25), accent, accent.withAlphaComponent(0.25),
    ]

    /// Chip behind the hovered-link hint at the card's bottom edge.
    static let statusChipBackground = dynamic(
        NSColor(red: 1, green: 1, blue: 1, alpha: 0.88),
        NSColor(red: 0.16, green: 0.16, blue: 0.20, alpha: 0.92))

    /// Button glyph/label colors and their disabled dimming.
    static let buttonGlyph = chromeText
    static let buttonGlyphDisabled = dynamic(
        NSColor(red: 0.68, green: 0.68, blue: 0.70, alpha: 1),
        NSColor(red: 0.42, green: 0.42, blue: 0.48, alpha: 1))

    /// Stable hue per title so every site keeps its own monogram color.
    static func tint(for title: String) -> NSColor {
        var hash = 5381
        for byte in title.utf8 { hash = (hash &* 33) &+ Int(byte) }
        let hue = CGFloat(abs(hash % 360)) / 360
        return dynamic(
            NSColor(hue: hue, saturation: 0.52, brightness: 0.82, alpha: 1),
            NSColor(hue: hue, saturation: 0.55, brightness: 1.0, alpha: 1))
    }

    // MARK: Typography
    // Chrome uses SF Pro (the system face) at a few fixed sizes; page text
    // sizes arrive from the engine and are rendered with Core Text.

    static let fontAddress = NSFont.systemFont(ofSize: 14, weight: .regular)
    static let fontTabLabel = NSFont.systemFont(ofSize: 12, weight: .medium)
    static let fontButton = NSFont.systemFont(ofSize: 12, weight: .regular)
    static let fontButtonMedium = NSFont.systemFont(ofSize: 12, weight: .medium)
    static let fontSidebarHeader = NSFont.systemFont(ofSize: 11, weight: .semibold)
    static let fontSectionLabel = NSFont.systemFont(ofSize: 11, weight: .regular)
    static let fontCellURL = NSFont.systemFont(ofSize: 10.5, weight: .regular)
    static let fontPaletteTitle = NSFont.systemFont(ofSize: 13, weight: .regular)
    static let fontSettingsTitle = NSFont.systemFont(ofSize: 17, weight: .semibold)
    static let fontSettingsRow = NSFont.systemFont(ofSize: 13, weight: .regular)
    static let fontSettingsRowDetail = NSFont.systemFont(ofSize: 11, weight: .regular)

    // MARK: Metrics

    static let topBarHeight: CGFloat = 56
    static let sidebarWidth: CGFloat = 248
    static let tabWidth: CGFloat = 184
    static let tabHeight: CGFloat = 30
    static let tabBarHeight: CGFloat = 40
    static let addressFieldHeight: CGFloat = 44
    static let pillHeight: CGFloat = 26
    static let cornerRadius: CGFloat = 7
    static let smallCornerRadius: CGFloat = 5
    static let pillCornerRadius: CGFloat = 10
    static let cardCornerRadius: CGFloat = 12
    /// Margins around the floating content card.
    static let cardInset: CGFloat = 10
    static let cardTopGap: CGFloat = 6
    static let hairlineThickness: CGFloat = 1

    static let paddingSmall: CGFloat = 6
    static let paddingMedium: CGFloat = 10
    static let paddingLarge: CGFloat = 16

    // MARK: Motion

    /// Standard ease for micro-animations (swift-out with a soft landing).
    static let animationTiming = CAMediaTimingFunction(name: .easeOut)
    /// A touch springier ease for entrances (palette, panes).
    static let springTiming = CAMediaTimingFunction(controlPoints: 0.2, 0.9, 0.3, 1.0)
    static let animationDuration: TimeInterval = 0.18
    static let animationDurationSlow: TimeInterval = 0.28
    static let animationDurationEntrance: TimeInterval = 0.34
    static let pressScale: CGFloat = 0.92
    static let hoverScale: CGFloat = 1.03

    /// All chrome animation routes through this gate: under System Settings →
    /// Accessibility → Display → Reduce Motion — or when the user prefers
    /// reduced motion in Kitae's Settings — every transition becomes instant.
    /// Views re-check `enabled` at animation time so the setting is live.
    static var motionEnabled: Bool {
        !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion && !motionReducedByUser
    }

    static var motionReducedByUser: Bool = SettingsStore.shared.motionReduced

    /// Run `changes` inside an animation context sized for a micro-animation.
    static func animate(_ changes: () -> Void) {
        guard motionEnabled else {
            changes()
            return
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = animationDuration
            context.timingFunction = animationTiming
            context.allowsImplicitAnimation = true
            changes()
        }
    }

    /// Entrance animation context (palette, settings panes).
    static func animateEntrance(_ changes: () -> Void) {
        guard motionEnabled else {
            changes()
            return
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = animationDurationEntrance
            context.timingFunction = springTiming
            context.allowsImplicitAnimation = true
            changes()
        }
    }

    /// A quick whole-view "boop" — the fun little bounce when a button fires.
    static func bounce(_ view: NSView) {
        guard motionEnabled, let layer = view.layer else { return }
        let anim = CAKeyframeAnimation(keyPath: "transform.scale")
        anim.values = [1.0, 0.88, 1.07, 1.0]
        anim.keyTimes = [0, 0.4, 0.75, 1]
        anim.duration = 0.34
        anim.timingFunction = animationTiming
        layer.add(anim, forKey: "bounce")
    }

    /// AppKit has no `resolvedColor(with:)` on NSColor (that's UIKit); a
    /// dynamic provider color resolves its CGColor against the *current*
    /// appearance, so perform the wanted appearance while asking.
    static func cgColor(of color: NSColor, for appearance: NSAppearance) -> CGColor {
        var resolved = color.cgColor
        appearance.performAsCurrentDrawingAppearance { resolved = color.cgColor }
        return resolved
    }
}
