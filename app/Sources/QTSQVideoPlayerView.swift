// QTSQVideoPlayerView.swift — the native transport for QTSQ video.
//
// The engine decodes frames and paints the current one into the page (the
// `<video>` element's image payload); this bar hosts only the transport.
// There is no local decoder: "playing" means a wall-clock anchor advances
// the position, and each step is pushed to the engine mirror through
// KITAE_CMD_MEDIA_PLAY so the painted frame follows the clock. State flows
// in via update(media:) from the active tab's frame; frames that merely
// echo what we last sent are suppressed, like the audio bar.

import AppKit

final class QTSQVideoPlayerView: NSView {

    /// Local transport change → the embedder pushes it to the engine mirror
    /// (command 16: playing flag + position seconds).
    var onTransport: ((Bool, Float) -> Void)?

    private var currentURL = ""
    private var mediaDuration: Float = 0

    /// Transport state. While playing, `anchor` ties the position to wall
    /// time; the tick timer pushes stepped positions to the engine.
    private var playing = false
    private var position: Float = 0
    private var anchor: (position: Float, at: Date)?

    private let playButton = GlyphButton(symbolName: "play.fill", glyphSize: 11) {}
    private let pauseButton = GlyphButton(symbolName: "pause.fill", glyphSize: 11) {}
    private let scrubber = MediaScrubber()
    private let elapsedLabel = NSTextField(labelWithString: "0:00")
    private let durationLabel = NSTextField(labelWithString: "0:00")
    private let filmIcon = NSImageView()
    private var tick: Timer?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true

        if let base = NSImage(systemSymbolName: "video.fill",
                              accessibilityDescription: "Video")?
            .withSymbolConfiguration(.init(pointSize: 12, weight: .medium)) {
            filmIcon.image = base
            filmIcon.contentTintColor = Theme.chromeMutedText
        }

        playButton.setAccessibilityLabel("Play video")
        pauseButton.setAccessibilityLabel("Pause video")
        scrubber.setAccessibilityLabel("Video playback position")
        playButton.action = { [weak self] in self?.togglePlay() }
        pauseButton.action = { [weak self] in self?.togglePlay() }
        pauseButton.isHidden = true

        elapsedLabel.font = Theme.fontSectionLabel
        elapsedLabel.textColor = Theme.chromeMutedText
        durationLabel.font = Theme.fontSectionLabel
        durationLabel.textColor = Theme.chromeMutedText

        scrubber.onScrub = { [weak self] value, finished in
            self?.scrubbed(to: value, finished: finished)
        }

        addSubview(filmIcon)
        addSubview(playButton)
        addSubview(pauseButton)
        addSubview(elapsedLabel)
        addSubview(scrubber)
        addSubview(durationLabel)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    deinit {
        tick?.invalidate()
    }

    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        Theme.statusChipBackground.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: Theme.pillCornerRadius,
                     yRadius: Theme.pillCornerRadius).fill()
        Theme.hairline.setStroke()
        let border = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5),
                                  xRadius: Theme.pillCornerRadius,
                                  yRadius: Theme.pillCornerRadius)
        border.lineWidth = 1
        border.stroke()
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        let btn: CGFloat = 26
        filmIcon.frame = NSRect(x: 16, y: (h - 16) / 2, width: 16, height: 16)
        let playFrame = NSRect(x: 44, y: (h - btn) / 2, width: btn, height: btn)
        playButton.frame = playFrame
        pauseButton.frame = playFrame
        elapsedLabel.frame = NSRect(x: 82, y: (h - 14) / 2, width: 38, height: 14)
        let durationW: CGFloat = 38
        durationLabel.frame = NSRect(x: bounds.width - durationW - 16,
                                     y: (h - 14) / 2, width: durationW, height: 14)
        scrubber.frame = NSRect(x: 128, y: (h - 14) / 2,
                                width: bounds.width - 128 - durationW - 30, height: 14)
    }

    // MARK: Frame-driven state

    /// Apply the media mirror published with the active tab's frame.
    func update(media: EngineFrame.MediaInfo) {
        if media.url != currentURL || (mediaDuration <= 0 && media.duration > 0) {
            load(url: media.url, duration: media.duration)
        }
        guard media.url == currentURL else { return }

        // Echo suppression: while a local command is in flight, frames that
        // don't yet reflect it are pre-echo; the matching one confirms it.
        if let echo = pendingEcho {
            if echo.playing == media.playing, abs(echo.position - media.position) < 0.05 {
                pendingEcho = nil
                return
            }
            if Date().timeIntervalSince(echo.at) < 2 { return }
            pendingEcho = nil
        }

        if media.playing == playing {
            // Local clock is authoritative while playing; when both paused
            // the mirror's position wins (state restored by a reload).
            if !playing, abs(media.position - position) > 0.02 {
                position = media.position
                refreshClockUI()
            }
        } else if media.playing {
            // Flipped to playing elsewhere (history restore): adopt it.
            play(from: media.position, notify: false)
        } else {
            // Flipped to paused elsewhere: stop at the mirror's position.
            pause(at: media.position, notify: false)
        }
    }

    /// The bar is being hidden (navigation away, tab switch): stop honestly —
    /// tell the engine we paused so its mirror stays truthful.
    func detach() {
        tick?.invalidate()
        tick = nil
        if playing {
            playing = false
            anchor = nil
            pendingEcho = (false, position, Date())
            onTransport?(false, position)
        }
        playing = false
        anchor = nil
        currentURL = ""
        mediaDuration = 0
        position = 0
        syncButtons()
    }

    private var pendingEcho: (playing: Bool, position: Float, at: Date)?

    // MARK: Internals

    private func load(url: String, duration: Float) {
        tick?.invalidate()
        tick = nil
        currentURL = url
        mediaDuration = duration
        playing = false
        anchor = nil
        position = 0
        pendingEcho = nil
        scrubber.value = 0
        scrubber.enabled = duration > 0
        playButton.isEnabled = duration > 0
        refreshClockUI()
        syncButtons()
    }

    private func togglePlay() {
        guard mediaDuration > 0 else { return }
        if playing {
            pause(at: currentPosition(), notify: true)
        } else {
            // Replay from the top once the end was reached.
            let start = position >= mediaDuration && mediaDuration > 0 ? 0 : position
            play(from: start, notify: true)
        }
    }

    private func play(from start: Float, notify: Bool) {
        guard mediaDuration > 0 else { return }
        position = start
        playing = true
        anchor = (start, Date())
        startTick()
        syncButtons()
        refreshClockUI()
        if notify {
            pendingEcho = (true, start, Date())
            onTransport?(true, start)
        }
    }

    private func pause(at at_: Float, notify: Bool) {
        position = min(max(0, at_), max(0, mediaDuration))
        playing = false
        anchor = nil
        tick?.invalidate()
        tick = nil
        syncButtons()
        refreshClockUI()
        if notify {
            pendingEcho = (false, position, Date())
            onTransport?(false, position)
        }
    }

    private func scrubbed(to value: CGFloat, finished: Bool) {
        guard mediaDuration > 0 else { return }
        let target = Float(value) * max(0, mediaDuration)
        if playing {
            // Keep rolling from the new spot; push each intermediate drag so
            // the picture follows the thumb even before release.
            anchor = (target, Date())
            if finished {
                pendingEcho = (true, target, Date())
                onTransport?(true, target)
            } else {
                onTransport?(true, target)
            }
        } else {
            position = target
            refreshClockUI()
            guard finished else {
                onTransport?(false, target)
                return
            }
            pendingEcho = (false, target, Date())
            onTransport?(false, target)
        }
    }

    private func currentPosition() -> Float {
        guard mediaDuration > 0 else { return position }
        guard let anchor else { return position }
        let elapsed = Float(Date().timeIntervalSince(anchor.at))
        return min(anchor.position + elapsed, max(0, mediaDuration))
    }

    private func startTick() {
        let t = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            self?.tickFired()
        }
        RunLoop.main.add(t, forMode: .common)
        tick = t
    }

    private func tickFired() {
        guard playing, !scrubber.isScrubbing else { return }
        let now = currentPosition()

        // Natural end: stop at the last frame; replay starts from the top
        // (togglePlay handles that).
        if mediaDuration > 0, now >= mediaDuration {
            pause(at: mediaDuration, notify: true)
            return
        }

        // Push the stepped position so the engine's painted frame follows
        // the clock. The echo of each push is suppressed in update(media:).
        position = now
        refreshClockUI()
        onTransport?(true, now)
    }

    private func syncButtons() {
        playButton.isHidden = playing
        pauseButton.isHidden = !playing
    }

    private func refreshClockUI() {
        let pos = currentPosition()
        scrubber.value = mediaDuration > 0 ? CGFloat(pos / mediaDuration) : 0
        scrubber.setAccessibilityValue("\(format(pos)) of \(format(mediaDuration))")
        scrubber.setAccessibilityHelp(playing ? "Playing" : "Paused")
        elapsedLabel.stringValue = format(pos)
        durationLabel.stringValue = format(mediaDuration)
    }

    private func format(_ seconds: Float) -> String {
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
