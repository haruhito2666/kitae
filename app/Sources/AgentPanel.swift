// AgentPanel.swift — B4: the in-app agent runtime, made visible.
//
// The app runs the *same* verbs kitae_mcp and kitae_server run (navigate,
// click, fill, submit, read, links, find, extract, watch, dispatch, back,
// screenshot, pdf, archive, verify), but on the browser it is already
// showing — the user's own tab, with their scroll position, focus and page
// scripts. This panel is what makes that legible:
//
//  * **Activity** — every act, in order, including the ones the permission
//    policy refused. An audit that only lists successes is not an audit, and
//    an agent the user cannot watch is a batch job with extra steps.
//  * **The prompt** — a verb outside the session's read-only rule comes back
//    `pending`; the panel asks, and on "Allow" it grants the verb and
//    re-issues the very request it was holding. Nothing is silently allowed:
//    the session starts read-only, and a refusal names the verb.
//  * **A trigger** — quick actions for the read/extract verbs an agent
//    usually starts with, a selector field for click/watch/find, and a
//    free-form JSON field for anything else (the whole vocabulary is
//    `kitae_agent::Verb::to_json`).
//
// The panel holds no engine reference: it sends through closures the host
// wires to the *active tab's* EngineController, so switching tabs switches
// which browser the agent drives.

import AppKit

/// One row of the engine's activity journal.
struct AgentJournalRow {
    let seq: Int
    let verb: String
    let denied: String?
    let outcome: String
    let elapsedMs: Int

    /// `#12 navigate — ok — https://… (412 ms)`, with refusals marked.
    var line: String {
        var text = "#\(seq) \(verb) — \(outcome)"
        if let denied, !denied.isEmpty {
            text = "#\(seq) \(verb) — refused: \(denied)"
        }
        return "\(text)  (\(elapsedMs) ms)"
    }
}

final class AgentPanel: NSObject, NSTextFieldDelegate {
    /// Send one verb, as its wire JSON.
    var onRequest: ((String) -> Void)?
    /// Record the user's approval for one verb ("*" = everything).
    var onGrant: ((String) -> Void)?
    /// Forget every grant.
    var onRevoke: (() -> Void)?
    /// Ask for the whole journal (panel open, or after a policy change).
    var onJournal: (() -> Void)?

    private let window: NSPanel
    private let activity: NSTextView
    private let result: NSTextView
    private let requestField: NSTextField
    private let statusLabel: NSTextField
    private let grantsLabel: NSTextField

    /// The request a prompt is holding: re-issued verbatim once allowed.
    private var pendingRequest: String?
    private var journalLines: [String] = []
    private var lastResult = ""

    override init() {
        window = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 460, height: 620),
            styleMask: [.titled, .closable, .resizable, .utilityWindow, .nonactivatingPanel],
            backing: .buffered, defer: false)
        window.title = "Agent"
        window.isReleasedWhenClosed = false

        let activityView = NSTextView(frame: .zero)
        activityView.isEditable = false
        activityView.isRichText = false
        activityView.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        activityView.isVerticallyResizable = true
        activityView.autoresizingMask = [.width]
        activityView.textContainer?.widthTracksTextView = true
        activity = activityView

        let resultView = NSTextView(frame: .zero)
        resultView.isEditable = false
        resultView.isRichText = false
        resultView.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        resultView.isVerticallyResizable = true
        resultView.autoresizingMask = [.width]
        resultView.textContainer?.widthTracksTextView = true
        result = resultView

        // Every stored property is assigned before `super.init()`: the ones
        // below reference no `self`, so Swift lets them come first and the
        // view assembly (which hands `self` out as a button target) can run
        // afterwards.
        let status = NSTextField(labelWithString: "Ready. Read-only verbs run as you ask; anything else asks first.")
        status.font = NSFont.systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.lineBreakMode = .byWordWrapping
        status.maximumNumberOfLines = 3
        statusLabel = status

        let grants = NSTextField(labelWithString: "Allowed: read-only verbs")
        grants.font = NSFont.systemFont(ofSize: 11)
        grants.textColor = .secondaryLabelColor
        grants.lineBreakMode = .byTruncatingTail
        grantsLabel = grants

        let field = NSTextField(string: "")
        field.placeholderString = "selector, or a verb's JSON — e.g. {\"verb\":\"click\",\"target\":{\"selector\":\"#go\"}}"
        field.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        requestField = field
        super.init()
        field.target = self
        field.action = #selector(submitField(_:))

        let activityScroll = AgentPanel.scroll(around: activityView, height: 220)
        let resultScroll = AgentPanel.scroll(around: resultView, height: 200)

        let rows: [NSView] = [
            Self.actions(titles: ["Read page", "Links", "Extract", "Screenshot…"],
                         target: self, selectors: [#selector(readPage(_:)), #selector(listLinks(_:)),
                                                   #selector(extractPage(_:)), #selector(screenshot(_:))]),
            Self.actions(titles: ["Find", "Click", "Watch", "Back"],
                         target: self, selectors: [#selector(findInPage(_:)), #selector(clickTarget(_:)),
                                                   #selector(watchTarget(_:)), #selector(goBack(_:))]),
            requestField,
            Self.actions(titles: ["Send JSON", "Reload panel", "Reset permissions"],
                         target: self, selectors: [#selector(submitField(_:)), #selector(refresh(_:)),
                                                   #selector(resetPermissions(_:))]),
            grantsLabel,
            Self.label("Activity (newest last)"),
            activityScroll,
            Self.label("Last result"),
            resultScroll,
            statusLabel,
        ]
        let stack = NSStackView(views: rows)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 6
        stack.edgeInsets = NSEdgeInsets(top: 10, left: 12, bottom: 10, right: 12)
        stack.translatesAutoresizingMaskIntoConstraints = false

        let content = NSView(frame: window.contentRect(forFrameRect: window.frame))
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            activityScroll.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -24),
            resultScroll.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -24),
            requestField.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -24),
            statusLabel.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -24),
            grantsLabel.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -24),
        ])
        window.contentView = content
    }

    // MARK: Building blocks

    private static func scroll(around view: NSTextView, height: CGFloat) -> NSScrollView {
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 420, height: height))
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.autoresizingMask = [.width]
        scroll.documentView = view
        scroll.heightAnchor.constraint(equalToConstant: height).isActive = true
        return scroll
    }

    private static func label(_ text: String) -> NSTextField {
        let label = NSTextField(labelWithString: text)
        label.font = NSFont.systemFont(ofSize: 11, weight: .semibold)
        return label
    }

    private static func actions(titles: [String], target: AnyObject, selectors: [Selector]) -> NSView {
        let buttons = zip(titles, selectors).map { title, selector -> NSButton in
            let button = NSButton(title: title, target: target, action: selector)
            button.bezelStyle = .rounded
            button.controlSize = .small
            return button
        }
        let row = NSStackView(views: buttons)
        row.orientation = .horizontal
        row.spacing = 6
        return row
    }

    // MARK: Visibility

    var isVisible: Bool { window.isVisible }

    func show(in parent: NSWindow?) {
        if let parent {
            parent.addChildWindow(window, ordered: .above)
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
        onJournal?()
    }

    func hide() {
        window.orderOut(nil)
    }

    // MARK: The engine's answers

    /// One agent reply: `{ok, verb, result, journal}` or `{ok:false, verb,
    /// kind, message, pending}`.
    func apply(reply: String) {
        guard let object = Self.parse(reply) else {
            statusLabel.stringValue = "The engine's reply was not JSON."; return
        }
        let verb = object["verb"] as? String ?? ""
        if let rows = object["journal"] as? [[String: Any]] {
            for row in rows { journalLines.append(Self.row(from: row).line) }
            renderActivity()
        }
        if object["ok"] as? Bool == true {
            pendingRequest = nil
            let result = object["result"]
            lastResult = result.flatMap { Self.pretty($0) } ?? "(no result)"
            renderResult()
            statusLabel.stringValue = "\(verb) ran."
            return
        }
        let message = object["message"] as? String ?? "the verb failed"
        switch object["kind"] as? String {
        case "permission" where object["pending"] as? Bool == true:
            lastResult = message
            renderResult()
            // The request that asked for this is re-issued verbatim if the
            // user allows — the engine has already journaled the refusal.
            ask(message: message, verb: verb)
        default:
            lastResult = message
            renderResult()
            statusLabel.stringValue = "\(verb) failed: \(message)"
            pendingRequest = nil
        }
    }

    /// The whole journal (oldest first).
    func apply(journal: String) {
        guard let array = Self.parseArray(journal) else { return }
        journalLines = array.map { Self.row(from: $0).line }
        renderActivity()
    }

    /// `{"grants":[…],"declared":[…]}`.
    func apply(policy: String) {
        guard let object = Self.parse(policy) else { return }
        let grants = object["grants"] as? [String] ?? []
        let declared = object["declared"] as? [String] ?? []
        let allowed = grants.contains("*")
            ? "everything"
            : (grants.isEmpty ? "read-only verbs" : grants.joined(separator: ", "))
        var text = "Allowed: \(allowed)"
        if !declared.isEmpty {
            text += "  •  declared: \(declared.prefix(8).joined(separator: ", "))"
        }
        grantsLabel.stringValue = text
    }

    // MARK: Actions

    @objc private func readPage(_ sender: Any?) { send(#"{"verb":"read"}"#) }
    @objc private func listLinks(_ sender: Any?) { send(#"{"verb":"links"}"#) }
    @objc private func extractPage(_ sender: Any?) { send(#"{"verb":"extract"}"#) }
    @objc private func goBack(_ sender: Any?) { send(#"{"verb":"back"}"#) }

    @objc private func screenshot(_ sender: Any?) {
        // Written to a file rather than carried inline: the reply then names
        // the artifact instead of shipping a base64 PNG through the panel.
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "kitae-agent.png"
        panel.allowedContentTypes = []
        panel.beginSheetModal(for: window) { response in
            guard response == .OK, let path = panel.url?.path else { return }
            self.send(#"{"verb":"screenshot","path":"\#(Self.escape(path))"}"#)
        }
    }

    @objc private func findInPage(_ sender: Any?) {
        let query = requestField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty, !query.hasPrefix("{") else {
            statusLabel.stringValue = "Type what to find in the field first."
            return
        }
        send(#"{"verb":"find","query":"\#(Self.escape(query))"}"#)
    }

    @objc private func clickTarget(_ sender: Any?) {
        let selector = requestField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !selector.isEmpty, !selector.hasPrefix("{") else {
            statusLabel.stringValue = "Type a CSS selector in the field first."
            return
        }
        send(#"{"verb":"click","target":{"selector":"\#(Self.escape(selector))"}}"#)
    }

    @objc private func watchTarget(_ sender: Any?) {
        let selector = requestField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !selector.isEmpty, !selector.hasPrefix("{") else {
            statusLabel.stringValue = "Type a CSS selector in the field first."
            return
        }
        send(#"{"verb":"watch","selector":"\#(Self.escape(selector))","timeoutMs":5000}"#)
    }

    /// The field takes either a verb's JSON or a bare verb name.
    @objc private func submitField(_ sender: Any?) {
        let text = requestField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        send(text.hasPrefix("{") ? text : "{\"verb\":\"\(Self.escape(text))\"}")
    }

    @objc private func refresh(_ sender: Any?) { onJournal?() }

    @objc private func resetPermissions(_ sender: Any?) {
        onRevoke?()
        statusLabel.stringValue = "Permissions reset — the session is read-only again."
    }

    // MARK: Internals

    private func send(_ request: String) {
        pendingRequest = request
        statusLabel.stringValue = "Running \(request)…"
        onRequest?(request)
    }

    private func ask(message: String, verb: String) {
        let alert = NSAlert()
        alert.messageText = "Allow this page action?"
        alert.informativeText = """
            The agent asked to run `\(verb)` on the page in this tab.

            \(message)

            Allowing applies to `\(verb)` for this session; the request is \
            re-issued immediately.
            """
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Allow \(verb)")
        alert.addButton(withTitle: "Don't Allow")
        let held = pendingRequest
        alert.beginSheetModal(for: window) { response in
            guard response == .alertFirstButtonReturn else {
                self.statusLabel.stringValue = "Refused `\(verb)`. The refusal is in the activity list."
                self.pendingRequest = nil
                return
            }
            self.onGrant?(verb)
            if let held {
                self.send(held)
            }
        }
    }

    private func renderActivity() {
        activity.string = journalLines.joined(separator: "\n")
        activity.scrollToEndOfDocument(nil)
    }

    private func renderResult() {
        result.string = lastResult
        result.scrollToBeginningOfDocument(nil)
    }

    /// Tested by NativeContractTests (no panel instance needed).
    static func parse(_ json: String) -> [String: Any]? {
        guard let data = json.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) else { return nil }
        return object as? [String: Any]
    }

    static func parseArray(_ json: String) -> [[String: Any]]? {
        guard let data = json.data(using: .utf8),
              let array = try? JSONSerialization.jsonObject(with: data) else { return nil }
        return array as? [[String: Any]]
    }

    static func row(from object: [String: Any]) -> AgentJournalRow {
        AgentJournalRow(
            seq: object["seq"] as? Int ?? 0,
            verb: object["verb"] as? String ?? "?",
            denied: object["denied"] as? String,
            outcome: object["outcome"] as? String ?? "",
            elapsedMs: object["elapsedMs"] as? Int ?? 0)
    }

    private static func pretty(_ value: Any) -> String {
        guard let data = try? JSONSerialization.data(
            withJSONObject: value, options: [.prettyPrinted, .sortedKeys]) else {
            return String(describing: value)
        }
        return String(data: data, encoding: .utf8) ?? ""
    }

    /// JSON string escaping for the values the panel interpolates (paths and
    /// selectors are the user's text, not ours).
    static func escape(_ text: String) -> String {
        var out = ""
        for character in text.unicodeScalars {
            switch character {
            case "\\": out += "\\\\"
            case "\"": out += "\\\""
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if character.value < 0x20 {
                    out += String(format: "\\u%04x", character.value)
                } else {
                    out.unicodeScalars.append(character)
                }
            }
        }
        return out
    }
}
