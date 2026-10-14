// EngineController.swift — Swift's owner of one KitaeEngine.
//
// Each controller spawns an engine (one OS thread inside libkitae_core.a),
// feeds it commands, polls serialized frames on the main run loop, and
// decodes them into EngineFrames. Text measurement is injected through the
// C vtable: Core Text measures on the engine thread so layout widths match
// exactly what PageCanvasView paints.

import AppKit
import CoreText

/// Core Text implementation of the host measurer contract.
///
/// Shared by every engine in the process. All entry points are thread-safe
/// (the engine calls them from its own thread): font lookup and the width
/// cache sit behind one lock, and caches are bounded.
final class CoreTextMeasurer {
    static let shared = CoreTextMeasurer()

    private let lock = NSLock()
    private var fonts: [FontKey: CTFont] = [:]
    private var widths: [WidthKey: CGFloat] = [:]
    private let widthCacheLimit = 50_000

    private struct FontKey: Hashable {
        var size: Float
        var bold: Bool
        var italic: Bool
        var monospace: Bool
        var family: String
    }

    private struct WidthKey: Hashable {
        var text: String
        var font: FontKey
    }

    /// The exact font PageCanvasView paints a run with — one source of truth
    /// shared with layout measurement. `family` is the run's first
    /// non-generic `font-family` ("" = host default): a named family
    /// resolves through Core Text (registered `@font-face` faces included,
    /// Q11); empty falls back to Menlo for monospace and the system UI font
    /// otherwise.
    func font(size: Float, weight: UInt32, italic: Bool, monospace: Bool, family: String = "") -> CTFont {
        let key = FontKey(size: size, bold: weight >= 600, italic: italic, monospace: monospace,
                          family: family)
        lock.lock()
        if let cached = fonts[key] {
            lock.unlock()
            return cached
        }
        lock.unlock()

        let created: CTFont
        if !family.isEmpty {
            var named = CTFontCreateWithName(family as CFString, CGFloat(size), nil)
            if key.bold || key.italic {
                var traits = CTFontSymbolicTraits()
                if key.bold { traits.insert(.traitBold) }
                if key.italic { traits.insert(.traitItalic) }
                let masked = CTFontSymbolicTraits(rawValue: traits.rawValue)
                if let derived = CTFontCreateCopyWithSymbolicTraits(named, CGFloat(size), nil, masked, masked) {
                    named = derived
                }
            }
            created = named
        } else if monospace {
            var mono = CTFontCreateWithName("Menlo" as CFString, CGFloat(size), nil)
            if key.bold || key.italic {
                var traits = CTFontSymbolicTraits()
                if key.bold { traits.insert(.traitBold) }
                if key.italic { traits.insert(.traitItalic) }
                let masked = CTFontSymbolicTraits(rawValue: traits.rawValue)
                if let derived = CTFontCreateCopyWithSymbolicTraits(mono, CGFloat(size), nil, masked, masked) {
                    mono = derived
                }
            }
            created = mono
        } else {
            let base = CTFontCreateUIFontForLanguage(.system, CGFloat(size), nil)
                ?? CTFontCreateWithName("Helvetica Neue" as CFString, CGFloat(size), nil)
            var traits = CTFontSymbolicTraits()
            if key.bold { traits.insert(.traitBold) }
            if key.italic { traits.insert(.traitItalic) }
            if traits.isEmpty {
                created = base
            } else {
                let masked = CTFontSymbolicTraits(rawValue: traits.rawValue)
                created = CTFontCreateCopyWithSymbolicTraits(base, CGFloat(size), nil, masked, masked) ?? base
            }
        }

        lock.lock()
        fonts[key] = created
        lock.unlock()
        return created
    }

    /// Advance width of `text` in points.
    func measure(_ text: String, size: Float, weight: UInt32, italic: Bool, monospace: Bool,
                 family: String = "") -> Float {
        let key = FontKey(size: size, bold: weight >= 600, italic: italic, monospace: monospace,
                          family: family)
        let widthKey = WidthKey(text: text, font: key)
        lock.lock()
        if let cached = widths[widthKey] {
            lock.unlock()
            return Float(cached)
        }
        lock.unlock()

        let f = font(size: size, weight: weight, italic: italic, monospace: monospace, family: family)
        // No explicit kern attribute: the font's default kerning applies,
        // and PageCanvasView.cachedLine uses the same attributes so paint
        // widths always match these measurements.
        let attributed = NSAttributedString(string: text, attributes: [
            .font: f,
        ])
        let line = CTLineCreateWithAttributedString(attributed)
        let width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))

        lock.lock()
        if widths.count >= widthCacheLimit { widths.removeAll(keepingCapacity: true) }
        widths[widthKey] = width
        lock.unlock()
        return Float(width)
    }

    /// (ascent, descent) in points.
    func metrics(size: Float, monospace: Bool, family: String = "") -> (Float, Float) {
        let f = font(size: size, weight: 400, italic: false, monospace: monospace, family: family)
        return (Float(CTFontGetAscent(f)), Float(CTFontGetDescent(f)))
    }

    /// B9: x-height of the resolved face — the `ex` unit.
    func xHeight(size: Float, weight: UInt32, italic: Bool, monospace: Bool, family: String) -> Float {
        let f = font(size: size, weight: weight, italic: italic, monospace: monospace, family: family)
        return Float(CTFontGetXHeight(f))
    }
}

// The C vtable entry points. Uniquely-named file-private functions so the
// symbol table stays unambiguous.
private func kitaeMeasure(
    ctx: UnsafeMutableRawPointer?,
    text: UnsafePointer<CChar>?,
    size: Float,
    weight: UInt32,
    italic: Int32,
    monospace: Int32,
    family: UnsafePointer<CChar>?
) -> Float {
    guard let text else { return 0 }
    let value = String(cString: text)
    let fam = family.map { String(cString: $0) } ?? ""
    return CoreTextMeasurer.shared.measure(value, size: size, weight: weight,
                                           italic: italic != 0, monospace: monospace != 0,
                                           family: fam)
}

private func kitaeMetrics(
    ctx: UnsafeMutableRawPointer?,
    size: Float,
    monospace: Int32,
    ascent: UnsafeMutablePointer<Float>?,
    descent: UnsafeMutablePointer<Float>?,
    family: UnsafePointer<CChar>?
) {
    let fam = family.map { String(cString: $0) } ?? ""
    let (a, d) = CoreTextMeasurer.shared.metrics(size: size, monospace: monospace != 0, family: fam)
    ascent?.pointee = a
    descent?.pointee = d
}

private func kitaeXHeight(
    ctx: UnsafeMutableRawPointer?,
    size: Float,
    weight: UInt32,
    italic: Int32,
    monospace: Int32,
    family: UnsafePointer<CChar>?
) -> Float {
    let fam = family.map { String(cString: $0) } ?? ""
    return CoreTextMeasurer.shared.xHeight(size: size, weight: weight,
                                           italic: italic != 0, monospace: monospace != 0,
                                           family: fam)
}

protocol EngineDelegate: AnyObject {
    /// A fresh frame arrived (called on the main thread).
    func engineDidProduceFrame(_ engine: EngineController, frame: EngineFrame)
    /// The loading state flipped (called on the main thread).
    func engineDidUpdateLoading(_ engine: EngineController, loading: Bool)
    /// The engine queued an escape-hatch URL to open outside the app.
    func engineWantsExternalOpen(_ engine: EngineController, url: String)
    /// A Save-Page-as-QTSQ request finished (called on the main thread).
    /// `error` is nil and `path` names the archive written, or `Some`
    /// explains the failure.
    func engineDidFinishSave(_ engine: EngineController, path: String, error: String?)
    /// The engine recovered from an internal panic (S-5, SECURITY_AUDIT.md).
    /// The tab keeps its last frame; the user can reload.
    func engineDidCrash(_ engine: EngineController, reason: String)
    /// H1: a download narrated by this tab's engine (any tab's downloads
    /// funnel here; DownloadCenter merges the streams by id).
    func engineDidNotifyDownload(_ engine: EngineController, item: DownloadItem)
    /// H2: a clicked `input[type=file]` asks the host to run its open panel.
    /// The host answers asynchronously with `setFileFiles` (or ignores the
    /// request, e.g. while a sheet is already up).
    func engineDidRequestFilePick(_ engine: EngineController, elementId: String, multiple: Bool)
    /// H6: the page answered 401 with a Basic/Digest challenge — the tab
    /// shows the engine's sign-in page and the host should present its
    /// credential sheet, answering with `setAuth`.
    func engineDidRequestAuth(_ engine: EngineController, scheme: String, realm: String, url: String)
    /// H6: the loaded page carries a login form (`loginform\t{form}\t{user}`
    /// `\t{pass}` ids). The host can prefill from its Keychain store.
    func engineDidFindLogin(_ engine: EngineController, formId: String, userFieldId: String, passFieldId: String)
    /// H6: a filled password input was in a submitting form
    /// (`pwsave\t{origin}\t{user}\t{pass}`) — offer to save it.
    func engineDidSubmitLoginForm(_ engine: EngineController, origin: String, user: String, password: String)
    /// H5: an inspector pick on this engine's page hit an element
    /// (`inspect\t{node}`); the host asks for its report via `inspectInfo`.
    func engineDidPickInspect(_ engine: EngineController, node: Int)
    /// H5: the report for a picked element arrived (`inspectinfo\t…`).
    /// Multi-line, tab-free text ready to display verbatim.
    func engineDidReceiveInspectInfo(_ engine: EngineController, info: String)
}

extension EngineDelegate {
    /// Default: ignore save outcomes (only the share flow listens).
    func engineDidFinishSave(_ engine: EngineController, path: String, error: String?) {}
    /// Default: log nothing — the engine has already recovered.
    func engineDidCrash(_ engine: EngineController, reason: String) {}
    /// Default: ignore download progress.
    func engineDidNotifyDownload(_ engine: EngineController, item: DownloadItem) {}
    /// Default: ignore file-pick requests.
    func engineDidRequestFilePick(_ engine: EngineController, elementId: String, multiple: Bool) {}
    /// Default: ignore 401 challenges.
    func engineDidRequestAuth(_ engine: EngineController, scheme: String, realm: String, url: String) {}
    /// Default: ignore login-form offers.
    func engineDidFindLogin(_ engine: EngineController, formId: String, userFieldId: String, passFieldId: String) {}
    /// Default: ignore password-save offers.
    func engineDidSubmitLoginForm(_ engine: EngineController, origin: String, user: String, password: String) {}
    /// Default: ignore inspector picks.
    func engineDidPickInspect(_ engine: EngineController, node: Int) {}
    /// Default: ignore inspector reports.
    func engineDidReceiveInspectInfo(_ engine: EngineController, info: String) {}
    func engineDidRequestInsecureNavigation(_ engine: EngineController, url: String) {}
}

/// H1: one row in the Downloads panel, parsed from the engine's
/// `download\t{id}\t{state}\t{received}\t{total}\t{filename}\t{path}`
/// notification (state ∈ active|done|failed:<message>; total may be empty).
struct DownloadItem {
    let id: UInt32
    var state: String
    var received: UInt64
    var total: UInt64?
    let filename: String
    let path: String

    var isFinished: Bool { state == "done" }
    var isFailed: Bool { state.hasPrefix("failed") }
    var isActive: Bool { !isFinished && !isFailed }
    var failureMessage: String? {
        guard isFailed else { return nil }
        return String(state.dropFirst("failed:".count))
    }
    /// 0...1 when the length is known, nil otherwise (indeterminate bar).
    var fraction: Double? {
        guard let total, total > 0 else { return nil }
        return min(1.0, Double(received) / Double(total))
    }
}

/// One controller == one engine thread. Created, commanded and destroyed on
/// the main thread only (the C contract requires a single owning thread).
final class EngineController {
    private var engine: OpaquePointer?
    private(set) var isLoading = false
    private weak var delegate: EngineDelegate?

    /// Reused frame buffer: grows to the largest frame seen (start ~1 MiB).
    private var frameBuffer = [UInt8](repeating: 0, count: 1 << 20)
    /// M-3 (SECURITY_AUDIT.md): buffers below this ceiling are never shrunk —
    /// the base 1 MiB stays as the steady-state working size for small pages.
    private let frameBufferShrinkFloor = 1 << 20
    private let externalBufferLimit = 4096

    /// Raw bytes of the most recently consumed frame (for the GPU tile
    /// renderer — the decoded EngineFrame drops wire details it needs).
    private(set) var lastFrameBytes: [UInt8] = []

    /// Lazily created GPU render handle (kitae_render_gpu).
    private var gpuHandle: OpaquePointer?

    init(viewport: CGSize, delegate: EngineDelegate?) {
        self.delegate = delegate

        var vtable = KitaeMeasurerVtable()
        vtable.ctx = nil
        vtable.measure = kitaeMeasure
        vtable.metrics = kitaeMetrics
        vtable.x_height = kitaeXHeight
        engine = kitae_engine_new(Int32(max(1, viewport.width)), Int32(max(1, viewport.height)), &vtable)
    }

    deinit {
        if let engine { kitae_engine_destroy(engine) }
        if let gpuHandle { kru_gpu_shutdown(gpuHandle) }
    }

    func clearPageCaches() {
        if gpuHandle == nil {
            gpuHandle = kru_gpu_init()
        }
        if let gpuHandle {
            kru_gpu_clear_layers(gpuHandle)
        }
        lastFrameBytes.removeAll(keepingCapacity: false)
        frameBuffer = [UInt8](repeating: 0, count: frameBufferShrinkFloor)
    }

    // MARK: Commands

    func navigate(_ input: String) {
        sendNavigate(input)
    }

    func goBack() { command(KITAE_CMD_BACK) }
    func goForward() { command(KITAE_CMD_FORWARD) }
    func reload() { command(KITAE_CMD_RELOAD) }

    func click(x: Float, y: Float) { command(KITAE_CMD_CLICK, x, y) }

    func focusNext() { command(KITAE_CMD_FOCUS_NEXT) }
    func focusPrev() { command(KITAE_CMD_FOCUS_PREV) }
    func activateFocused() { command(KITAE_CMD_ACTIVATE) }
    func escape() { command(KITAE_CMD_ESCAPE) }
    func stepFocus(_ dir: Int) { command(KITAE_CMD_STEP_FOCUS, Float(dir)) }
    // A11a: caret + text editing for the focused editable control. The engine
    // owns the caret; these forward key events to it.
    func typeText(_ text: String) { sendString(KITAE_CMD_TYPE, text) }
    func backspace() { command(KITAE_CMD_BACKSPACE) }
    func deleteForward() { command(KITAE_CMD_DELETE_FORWARD) }
    func moveCaret(_ delta: Int) { command(KITAE_CMD_MOVE_CARET, Float(delta)) }
    func setCaret(_ offset: Int) { command(KITAE_CMD_SET_CARET, Float(offset)) }
    func extendCaret(_ delta: Int) { command(KITAE_CMD_EXTEND_CARET, Float(delta)) }
    func beginSelection(_ offset: Int) { command(KITAE_CMD_SELECTION_BEGIN, Float(offset)) }
    func extendSelection(_ offset: Int) { command(KITAE_CMD_SELECTION_EXTEND, Float(offset)) }
    func beginSelection(x: Float, y: Float) { command(KITAE_CMD_SELECTION_BEGIN_POINT, x, y) }
    func extendSelection(x: Float, y: Float) { command(KITAE_CMD_SELECTION_EXTEND_POINT, x, y) }
    func endSelection() { command(KITAE_CMD_SELECTION_END) }
    func updateMarkedText(_ text: String) { sendString(KITAE_CMD_COMPOSITION_UPDATE, text) }
    func commitComposition(_ text: String) { sendString(KITAE_CMD_COMPOSITION_COMMIT, text) }
    func cancelComposition() { command(KITAE_CMD_COMPOSITION_CANCEL) }
    func submitForm(_ id: String) { sendString(KITAE_CMD_SUBMIT, id) }
    func setInput(id: String, value: String) { sendString(KITAE_CMD_SET_INPUT, "\(id)\n\(value)") }

    /// H2: the open-panel answer for a clicked `input[type=file]`. Wire:
    /// "id\npath1\tpath2…" — an empty path list clears the pick (the engine
    /// mirrors the display into the control's value attribute itself).
    func setFileFiles(id: String, paths: [String]) {
        sendString(KITAE_CMD_SET_FILE_FILES, "\(id)\n\(paths.joined(separator: "\t"))")
    }

    /// H6: the sign-in sheet's answer to an `auth\t` notification. The
    /// engine stores the session credentials (its automatic 401 retry picks
    /// them up) and re-fetches the challenged URL.
    func setAuth(user: String, pass: String) {
        sendString(KITAE_CMD_SET_AUTH, "\(user)\n\(pass)")
    }

    /// H5: ask the engine which element sits at a page-space point (the
    /// canvas adds the scroll offset back). The hit — if any — arrives as an
    /// `inspect\t{node}` notification on this engine's queue.
    func inspectPick(x: Float, y: Float) {
        command(KITAE_CMD_INSPECT_PICK, x, y)
    }

    /// H5: ask for the element + computed-style report of a picked node.
    /// The multi-line text arrives as an `inspectinfo\t…` notification.
    func inspectInfo(node: Int) {
        command(KITAE_CMD_INSPECT_INFO, Float(node))
    }

    func selectAll() { command(KITAE_CMD_SELECT_ALL) }
    func clearSelection() { command(KITAE_CMD_CLEAR_SELECTION) }

    /// Hover/active node by element id for `:hover`/`:active` restyle
    /// (nil/empty clears). Swift mouse wiring lands with the accessibility
    /// tree (Phase P), which resolves points to ids; until then these are
    /// driven by id-addressed callers.
    func setHover(_ id: String?) { sendString(KITAE_CMD_HOVER, id ?? "") }
    func setActive(_ id: String?) { sendString(KITAE_CMD_ACTIVE, id ?? "") }

    // Find-in-page (P1): query, step, clear. Frames carry the matches +
    // active index; the canvas overlays the rects.
    func find(_ query: String) { sendString(KITAE_CMD_FIND, query) }
    func findNext() { command(KITAE_CMD_FIND_NEXT) }
    func findPrev() { command(KITAE_CMD_FIND_PREV) }
    func findClear() { command(KITAE_CMD_FIND_CLEAR) }

    // Media transport (J4): the engine decodes; the embedder plays.
    func mediaOpen() { command(KITAE_CMD_MEDIA_OPEN) }
    func mediaPlay(playing: Bool, position: Float) {
        command(KITAE_CMD_MEDIA_PLAY, playing ? 1 : 0, position)
    }
    func mediaState() { command(KITAE_CMD_MEDIA_STATE) }

    // QTSQ unlock: store the session password and reload the locked page.
    // The engine re-publishes a frame — unlocked when the password was right.
    func unlockQtsq(_ password: String) {
        sendString(KITAE_CMD_SET_PASSWORD, password)
    }

    // Environment media features (command 25): what `@media (prefers-…)` on
    // the page sees. Pages styled for dark mode / reduced motion re-render
    // in place — no reload.
    func setEnvironment(dark: Bool, reducedMotion: Bool) {
        command(KITAE_CMD_SET_ENVIRONMENT, dark ? 1 : 0, reducedMotion ? 1 : 0)
    }

    // H3 print media (command 52): the page is re-styled and re-laid-out for
    // `print` (its `@media print` blocks apply, `@media screen` ones stop) and
    // the reply frame carries `isPrintMedia`. `false` puts the screen frame
    // back — every caller must send it again when the print run is over.
    func setPrintMedia(_ on: Bool) {
        command(KITAE_CMD_SET_MEDIA, on ? 1 : 0)
    }

    /// Throw away the pending frame, if any, without adopting it. The print
    /// path drains before asking for print media so the frame it picks up is
    /// the print one, not a screen repaint that was already in the mailbox.
    @discardableResult
    func drainPendingFrame() -> Bool {
        guard let engine else { return false }
        var length = 0
        guard kitae_engine_frame_available(engine, &length) == 1, length > 0 else { return false }
        var scratch = [UInt8](repeating: 0, count: length)
        return scratch.withUnsafeMutableBytes { raw in
            kitae_engine_frame(engine, raw.bindMemory(to: UInt8.self).baseAddress, length) > 0
        }
    }

    func setPrivacyPolicy(dnt: Bool, blockThirdPartyCookies: Bool, httpsOnly: Bool,
                          warnInsecureHTTP: Bool) {
        var bits = 0
        if dnt { bits |= 1 }
        if blockThirdPartyCookies { bits |= 2 }
        if httpsOnly { bits |= 4 }
        if warnInsecureHTTP { bits |= 8 }
        command(KITAE_CMD_SET_PRIVACY_POLICY, Float(bits))
    }

    func resolveInsecureNavigation(allow: Bool) {
        command(KITAE_CMD_RESOLVE_INSECURE_NAVIGATION, allow ? 1 : 0)
    }

    func setSearchProvider(_ provider: SearchProviderSetting) {
        command(KITAE_CMD_SET_SEARCH_PROVIDER, Float(provider.rawValue))
    }

    // Save-Page-as-QTSQ (command 26): write the current page as a
    // device-encrypted archive at `path`. The outcome arrives later through
    // engineDidFinishSave.
    func savePageQtsq(to path: String) {
        sendString(KITAE_CMD_SAVE_PAGE, path)
    }

    func resize(to size: CGSize) {
        command(KITAE_CMD_RESIZE, Float(max(1, size.width)), Float(max(1, size.height)))
    }

    private func command(_ kind: Int, _ a: Float = 0, _ b: Float = 0) {
        guard let engine else { return }
        kitae_engine_command(engine, Int32(kind), a, b, nil)
    }

    private func sendNavigate(_ input: String) {
        guard let engine else { return }
        // [CChar] bridges to UnsafePointer<CChar> at the call site.
        let cString = Array(input.utf8CString)
        kitae_engine_command(engine, Int32(KITAE_CMD_NAVIGATE), 0, 0, cString)
    }

    // MARK: GPU tiles (Phase L5)

    /// Render the latest frame's layers to an RGBA8 tile through
    /// kitae_render_gpu (Metal when available, CPU reference fallback
    /// inside). Returns nil when the frame is missing or malformed.
    func renderGPUTile(width: Int, height: Int, scale: Float,
                       scroll: CGPoint) -> (pixels: [UInt8], w: Int, h: Int)? {
        let raw = lastFrameBytes
        guard !raw.isEmpty, width > 0, height > 0,
              width <= 4096, height <= 4096 else { return nil }
        if gpuHandle == nil {
            gpuHandle = kru_gpu_init()
        }
        guard let gpuHandle else { return nil }
        guard let ptr = raw.withUnsafeBytes({ buf -> UnsafeMutablePointer<UInt8>? in
            guard let base = buf.bindMemory(to: UInt8.self).baseAddress else { return nil }
            return kru_gpu_render(gpuHandle, base, raw.count,
                                  UInt32(width), UInt32(height),
                                  scale, Float(scroll.x), Float(scroll.y))
        }) else { return nil }
        let count = width * height * 4
        var out = [UInt8](repeating: 0, count: count)
        out.withUnsafeMutableBytes { dst in
            dst.baseAddress?.copyMemory(from: ptr, byteCount: count)
        }
        kru_gpu_pixels_free(ptr, count)
        return (out, width, height)
    }

    private func sendString(_ kind: Int, _ value: String) {
        guard let engine else { return }
        let cString = Array(value.utf8CString)
        kitae_engine_command(engine, Int32(kind), 0, 0, cString)
    }

    // MARK: Polling (main thread)

    /// Drain the engine's mailboxes: one frame, the loading flag, and any
    /// queued external URLs. Returns the frame when one was published.
    @discardableResult
    func poll() -> EngineFrame? {
        guard let engine else { return nil }

        var frame: EngineFrame?
        let length = frameBuffer.withUnsafeMutableBytes { raw -> Int in
            kitae_engine_frame(engine, raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
        if length > 0 {
            if length > frameBuffer.count {
                // Too big for the current buffer: the frame stayed pending.
                // Grow, pull it, then keep going.
                frameBuffer = [UInt8](repeating: 0, count: length + 4096)
                let _ = frameBuffer.withUnsafeMutableBytes { raw -> Int in
                    kitae_engine_frame(engine, raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
                }
            }
            frame = EngineFrame.decode(frameBuffer)
            if frame != nil {
                lastFrameBytes = Array(frameBuffer.prefix(length))
                // M-3 (SECURITY_AUDIT.md): don't retain the high-water buffer
                // forever. If the current frame is far smaller than the buffer
                // ceiling (more than 2x), shrink back to ~1 frame + slack, so
                // a tab that once rendered one heavy page doesn't hold MBs of
                // chrome memory at rest. The 2x hysteresis prevents thrash.
                if frameBuffer.count > frameBufferShrinkFloor && frameBuffer.count > length * 2 {
                    frameBuffer = [UInt8](repeating: 0, count: length + 4096)
                }
            }
        }

        let loading = kitae_engine_loading(engine) != 0
        if loading != isLoading {
            isLoading = loading
            delegate?.engineDidUpdateLoading(self, loading: loading)
        }

        drainExternals()
        drainNotifications()

        if let frame {
            delegate?.engineDidProduceFrame(self, frame: frame)
        }
        return frame
    }

    private func drainExternals() {
        guard let engine else { return }
        while true {
            var buf = [CChar](repeating: 0, count: externalBufferLimit)
            let got = buf.withUnsafeMutableBytes { raw -> Int32 in
                kitae_engine_take_external(engine, raw.bindMemory(to: CChar.self).baseAddress, raw.count)
            }
            guard got == 1 else { break }
            let url = String(cString: buf)
            if !url.isEmpty {
                delegate?.engineWantsExternalOpen(self, url: url)
            }
        }
    }

    /// Pop queued engine notifications — Save-Page-as-QTSQ outcomes, encoded
    /// as "1\t{path}" on success and "0\t{message}" on failure (the same wire
    /// the ffi.rs tests exercise), "crash\t{reason}" when the S-5 panic
    /// guard recovered inside the engine thread, and H1's
    /// "download\t{id}\t{state}\t{received}\t{total}\t{filename}\t{path}".
    private func drainNotifications() {
        guard let engine else { return }
        while true {
            var buf = [CChar](repeating: 0, count: externalBufferLimit)
            let got = buf.withUnsafeMutableBytes { raw -> Int32 in
                kitae_engine_take_notification(engine, raw.bindMemory(to: CChar.self).baseAddress, raw.count)
            }
            guard got == 1 else { break }
            let message = String(cString: buf)
            guard !message.isEmpty else { continue }
            if message.starts(with: "download\t") {
                let fields = message.components(separatedBy: "\t")
                // download, id, state, received, total, filename, path
                guard fields.count >= 7,
                      let id = UInt32(fields[1]),
                      let received = UInt64(fields[3])
                else { continue }
                let item = DownloadItem(
                    id: id,
                    state: fields[2],
                    received: received,
                    total: UInt64(fields[4]),
                    filename: fields[5],
                    path: fields.dropFirst(6).joined(separator: "\t")
                )
                delegate?.engineDidNotifyDownload(self, item: item)
                continue
            }
            if message.starts(with: "filepick\t") {
                // H2: "filepick\t{id-or-name}\t{0|1}" — 1 = the control has
                // `multiple`. The panel opens from the delegate; the answer
                // rides back as SET_FILE_FILES.
                let fields = message.components(separatedBy: "\t")
                guard fields.count >= 3 else { continue }
                delegate?.engineDidRequestFilePick(
                    self,
                    elementId: fields[1],
                    multiple: fields[2] == "1"
                )
                continue
            }
            if message.starts(with: "auth\t") {
                // H6: "auth\t{scheme}\t{realm}\t{url}" — present the
                // credential sheet for this challenged load.
                let fields = message.components(separatedBy: "\t")
                guard fields.count >= 4 else { continue }
                delegate?.engineDidRequestAuth(
                    self,
                    scheme: fields[1],
                    realm: fields[2],
                    url: fields.dropFirst(3).joined(separator: "\t")
                )
                continue
            }
            if message.starts(with: "loginform\t") {
                // H6: "loginform\t{form}\t{user}\t{pass}" — ids the host can
                // fill through setInput.
                let fields = message.components(separatedBy: "\t")
                guard fields.count >= 4 else { continue }
                delegate?.engineDidFindLogin(
                    self,
                    formId: fields[1],
                    userFieldId: fields[2],
                    passFieldId: fields[3]
                )
                continue
            }
            if message.starts(with: "pwsave\t") {
                // H6: "pwsave\t{origin}\t{user}\t{pass}" — the password may
                // itself contain tabs, so the tail is one field.
                let fields = message.components(separatedBy: "\t")
                guard fields.count >= 4 else { continue }
                delegate?.engineDidSubmitLoginForm(
                    self,
                    origin: fields[1],
                    user: fields[2],
                    password: fields.dropFirst(3).joined(separator: "\t")
                )
                continue
            }
            if message.starts(with: "inspect\t") {
                // H5: "inspect\t{node}" — an inspector pick hit an element.
                let fields = message.components(separatedBy: "\t")
                guard fields.count >= 2, let node = Int(fields[1]) else { continue }
                delegate?.engineDidPickInspect(self, node: node)
                continue
            }
            if message.starts(with: "inspectinfo\t") {
                // H5: "inspectinfo\t{lines…}" — the picked element's report;
                // the engine keeps tabs out of the payload, so this is one
                // field with embedded newlines.
                delegate?.engineDidReceiveInspectInfo(
                    self,
                    info: String(message.dropFirst("inspectinfo\t".count))
                )
                continue
            }
            if message.starts(with: "insecure\t") {
                delegate?.engineDidRequestInsecureNavigation(
                    self,
                    url: String(message.dropFirst("insecure\t".count))
                )
                continue
            }
            let tab = message.firstIndex(of: "\t")
            if message.starts(with: "crash\t") {
                let reason = tab.map { String(message[message.index(after: $0)...]) } ?? "engine panic"
                delegate?.engineDidCrash(self, reason: reason)
                continue
            }
            let ok = message.starts(with: "1\t")
            let payload = tab.map { String(message[message.index(after: $0)...]) } ?? message
            if ok {
                delegate?.engineDidFinishSave(self, path: payload, error: nil)
            } else {
                delegate?.engineDidFinishSave(self, path: "", error: payload.isEmpty ? "save failed" : payload)
            }
        }
    }
}
