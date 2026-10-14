// InspectorPanel.swift — H5: the read-only DOM + computed-CSS readout.
//
// One utility panel for the whole app: a ⌘-click on the active tab's page
// asks the engine which element sits under the point, and the engine's
// report (tag#id.classes, attributes, computed properties) is displayed
// verbatim here. The panel never edits the page — the engine owns it; this
// window only describes what the cascade produced.

import AppKit

final class InspectorPanel {
    private let window: NSPanel
    private let textView: NSTextView

    init() {
        window = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 400, height: 520),
            styleMask: [.titled, .closable, .utilityWindow, .nonactivatingPanel],
            backing: .buffered, defer: false)
        window.title = "Inspector"
        // Closing the panel hides it; the controller keeps it for reopening.
        window.isReleasedWhenClosed = false

        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 400, height: 520))
        scroll.autoresizingMask = [.width, .height]
        scroll.hasVerticalScroller = true
        scroll.borderType = .noBorder

        let text = NSTextView(frame: scroll.contentView.bounds)
        text.isEditable = false
        text.isRichText = false
        text.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        text.isVerticallyResizable = true
        text.autoresizingMask = [.width]
        text.textContainer?.containerSize = NSSize(
            width: scroll.contentSize.width, height: .greatestFiniteMagnitude)
        text.textContainer?.widthTracksTextView = true
        textView = text
        scroll.documentView = text
        window.contentView = scroll
    }

    var isVisible: Bool { window.isVisible }

    /// Show `info` (the engine's report, or a hint when the panel is opened
    /// without a pick). Titled after the report's first line.
    func show(in parent: NSWindow?, info: String) {
        textView.string = info
        let firstLine = info.split(separator: "\n", maxSplits: 1).first ?? "Inspector"
        window.title = firstLine.isEmpty ? "Inspector" : "Inspector — \(firstLine)"
        textView.scrollToBeginningOfDocument(nil)
        if let parent {
            parent.addChildWindow(window, ordered: .above)
            // Park right of the browser window, clamped to the screen.
            let screenFrame = parent.screen?.visibleFrame ?? NSScreen.main?.visibleFrame
            var x = parent.frame.maxX + 8
            if let screenFrame, x + window.frame.width > screenFrame.maxX {
                x = screenFrame.maxX - window.frame.width
            }
            var y = parent.frame.maxY - 40
            if let screenFrame, y - window.frame.height < screenFrame.minY {
                y = screenFrame.minY + window.frame.height
            }
            window.setFrameTopLeftPoint(NSPoint(x: x, y: y))
        }
        window.orderFront(nil)
    }

    func hide() {
        window.orderOut(nil)
    }
}
