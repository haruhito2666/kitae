// PagePrinter.swift — H3: print the active tab's page to PDF.
//
// The engine ships whole-document frames, so pagination is a pure host
// concern: an offscreen PageCanvasView sized to the full document paints
// through NSPrintOperation's automatic vertical pagination, so every page
// slice runs the exact painters the screen shows (rects, gradients,
// rasters, Core Text runs, clips, blend/isolation stamps). The view is
// sized to the document height, which also disables the overlay scrollbar
// and sticky shifting — neither belongs on paper.
//
// The frame printed is the one the engine lays out for *print media*
// (command 52): the engine is resized to the paper's printable width and
// switched to print media, the republished frame comes back carrying
// `isPrintMedia`, and only then does the print operation run. So a page's
// `@media print` block is what lands on paper, and `prefers-color-scheme` /
// `prefers-reduced-motion` answer as paper rather than as the screen. The
// tab is put back on screen media (and the screen viewport) whatever
// happens, and while the run is in flight `Tab.isPrinting` keeps the
// print-styled frame off the canvas.

import AppKit
import UniformTypeIdentifiers

enum PagePrinter {

    enum Paper: CaseIterable {
        case letter
        case a4

        var size: CGSize {
            switch self {
            case .letter: return CGSize(width: 612, height: 792)      // 8.5 × 11 in
            case .a4: return CGSize(width: 595.28, height: 841.89)    // 210 × 297 mm
            }
        }

        var label: String {
            switch self {
            case .letter: return "Letter"
            case .a4: return "A4"
            }
        }

        /// The content width the page is laid out at: the sheet minus the
        /// margins on both sides. This is the viewport print media sees, so
        /// `@media (max-width: …)` rules resolve against paper, not the window.
        var printableWidth: CGFloat { size.width - PagePrinter.margin * 2 }
    }

    private static let margin: CGFloat = 36 // 0.5 in on every edge

    /// How long to wait for the engine's print-media republish. A relayout of
    /// the page already in memory is milliseconds; the budget only covers a
    /// busy engine thread, and gives up rather than hanging the sheet.
    private static let layoutTimeout: TimeInterval = 5.0

    /// Write `frame`'s document to `url` as a paginated PDF. `pageWidth` is
    /// the layout viewport the frame was built for (the width its page-space
    /// coordinates span). Returns nil on success, else a failure message.
    @discardableResult
    static func export(frame: EngineFrame, to url: URL, pageWidth: CGFloat,
                       paper: Paper) -> String? {
        let contentHeight = CGFloat(frame.contentHeight)
        guard contentHeight > 0, pageWidth > 0 else {
            return "This page has nothing to print."
        }

        let canvas = PageCanvasView(
            frame: NSRect(x: 0, y: 0, width: pageWidth, height: contentHeight))
        canvas.applyFrame(frame)
        defer { canvas.stopAnimationClock() }

        let printInfo = NSPrintInfo()
        printInfo.paperSize = paper.size
        printInfo.topMargin = margin
        printInfo.bottomMargin = margin
        printInfo.leftMargin = margin
        printInfo.rightMargin = margin
        printInfo.horizontalPagination = .fit   // shrink the page width onto the paper
        printInfo.verticalPagination = .automatic
        printInfo.jobDisposition = .save
        printInfo.dictionary()[NSPrintInfo.AttributeKey.jobSavingURL] = url

        let operation = NSPrintOperation(view: canvas, printInfo: printInfo)
        operation.showsPrintPanel = false
        operation.showsProgressPanel = false
        guard operation.run(),
              FileManager.default.fileExists(atPath: url.path)
        else { return "The PDF could not be written." }
        return nil
    }

    /// The ⌘P front end: a save sheet (with a paper picker) over the active
    /// tab. On OK the page is re-laid-out in print media
    /// ([`exportPrintMedia`]) and that frame is written to the chosen PDF.
    /// Returns the error message on failure.
    static func printActiveTab(_ tab: Tab, from window: NSWindow?,
                               onResult: @escaping (String?) -> Void) {
        guard let frame = tab.latestFrame else {
            onResult("Nothing to print — this tab has no page yet.")
            return
        }
        guard let window else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.pdf]
        let base = frame.title.isEmpty ? "page" : frame.title
        panel.nameFieldStringValue =
            base.replacingOccurrences(of: "/", with: "-") + ".pdf"

        let paperPicker = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 120, height: 26),
                                        pullsDown: false)
        for paper in Paper.allCases { paperPicker.addItem(withTitle: paper.label) }
        let accessory = NSView(frame: NSRect(x: 0, y: 0, width: 220, height: 30))
        let label = NSTextField(labelWithString: "Paper:")
        label.frame = NSRect(x: 0, y: 6, width: 50, height: 18)
        paperPicker.frame = NSRect(x: 52, y: 2, width: 120, height: 26)
        accessory.addSubview(label)
        accessory.addSubview(paperPicker)
        panel.accessoryView = accessory

        panel.beginSheetModal(for: window) { response in
            guard response == .OK, let url = panel.url else { return }
            let paper = Paper.allCases[paperPicker.indexOfSelectedItem]
            onResult(exportPrintMedia(tab: tab, to: url, paper: paper))
        }
    }

    /// Print the tab's current page to `url` as PDF, in print media.
    ///
    /// The engine is resized to the paper's printable width and switched to
    /// print media, the frame that comes back is the one that gets printed,
    /// and screen media + the screen viewport are restored on every path out
    /// of here (including the timeout) — before the PDF is written, so the
    /// screen is already back on screen media while the print operation runs.
    /// Returns nil on success, else the message to show the user.
    @discardableResult
    static func exportPrintMedia(tab: Tab, to url: URL, paper: Paper) -> String? {
        // The print run owns the engine: no screen frame is adopted or
        // painted while it holds print media. The gate is released as soon as
        // the print frame is in hand, so the restored screen frame paints.
        tab.beginPrintRun()
        var gateHeld = true
        defer { if gateHeld { tab.endPrintRun() } }

        let printWidth = paper.printableWidth
        // Lay out at the paper's printable width, so `@media (max-width: …)`
        // resolves against paper rather than against the window.
        tab.engine.resize(
            to: CGSize(width: printWidth, height: max(1, tab.canvas.bounds.height)))
        // Drop any screen repaint already in the mailbox, so the next frame we
        // see is the engine's answer to the print switch.
        tab.engine.drainPendingFrame()
        tab.engine.setPrintMedia(true)

        let frame = capturePrintFrame(tab)
        tab.engine.setPrintMedia(false)
        // Back to whatever the canvas is now (the window may have moved while
        // we were on paper).
        tab.engine.resize(to: tab.canvas.bounds.size)
        tab.endPrintRun()
        gateHeld = false

        guard let frame else {
            return "The print layout did not finish in time."
        }
        return export(frame: frame, to: url, pageWidth: printWidth, paper: paper)
    }

    /// Spin the main run loop until the engine publishes a print-media frame.
    /// Frames that arrive first (a repaint that was already in flight) are
    /// dropped — `Tab.isPrinting` means the canvas is not missing anything.
    private static func capturePrintFrame(_ tab: Tab) -> EngineFrame? {
        let deadline = Date().addingTimeInterval(layoutTimeout)
        while Date() < deadline {
            if let frame = tab.engine.poll(), frame.isPrintMedia { return frame }
            RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
        }
        return nil
    }
}
