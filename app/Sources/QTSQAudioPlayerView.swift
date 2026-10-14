// QTSQAudioPlayerView.swift — the native transport for decoded QTSQ audio.
//
// The engine decodes (and hands off a temp CAF file); this view plays it
// with AVFoundation and mirrors transport changes back through the engine
// (KITAE_CMD_MEDIA_PLAY) so the mirror carried in every published frame
// stays truthful. Playback state flows in via update(media:) from the
// active tab's frame; the local AVAudioPlayer clock drives the scrubber
// while playing, since the engine mirror only records what we last sent.

import AppKit
import AVFoundation

// MARK: - Scrubber

/// The theme-styled progress slider: hairline track, accent fill, round
/// thumb. Reports (value 0…1, finished) so the player can live-update
/// labels while dragging and seek only on release.
final class MediaScrubber: NSView {

    var value: CGFloat = 0 { didSet {
        setAccessibilityValue(NSNumber(value: Double(value)))
        needsDisplay = true
    } }
    var enabled = true { didSet {
        setAccessibilityEnabled(enabled)
        needsDisplay = true
    } }
    var onScrub: ((CGFloat, Bool) -> Void)?
    private(set) var isScrubbing = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        setAccessibilityElement(true)
        setAccessibilityRole(.slider)
        setAccessibilityLabel("Playback position")
        setAccessibilityValue(NSNumber(value: 0.0))
        setAccessibilityEnabled(true)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    func accessibilityIncrement() {
        guard enabled else { return }
        value = min(1, value + 0.05)
        onScrub?(value, true)
    }

    func accessibilityDecrement() {
        guard enabled else { return }
        value = max(0, value - 0.05)
        onScrub?(value, true)
    }

    override func draw(_ dirtyRect: NSRect) {
        let trackH: CGFloat = 4
        let trackRect = NSRect(x: 0, y: (bounds.height - trackH) / 2,
                               width: bounds.width, height: trackH)
        Theme.hairline.setFill()
        NSBezierPath(roundedRect: trackRect, xRadius: 2, yRadius: 2).fill()

        if value > 0.001 {
            let fillRect = NSRect(x: 0, y: trackRect.origin.y,
                                  width: max(4, trackRect.width * min(1, value)),
                                  height: trackH)
            Theme.accent.setFill()
            NSBezierPath(roundedRect: fillRect, xRadius: 2, yRadius: 2).fill()
        }

        let r: CGFloat = enabled ? 5.5 : 4
        let cx = trackRect.width * min(1, max(0, value))
        let thumb = NSRect(x: cx - r, y: bounds.height / 2 - r, width: r * 2, height: r * 2)
        Theme.accent.setFill()
        NSBezierPath(ovalIn: thumb).fill()
        let rim = NSBezierPath(ovalIn: thumb.insetBy(dx: -1, dy: -1))
        rim.lineWidth = 1.5
        Theme.cardBackground.setStroke()
        rim.stroke()
    }

    override func mouseDown(with event: NSEvent) {
        guard enabled else { return }
        isScrubbing = true
        apply(event)
    }

    override func mouseDragged(with event: NSEvent) {
        guard isScrubbing else { return }
        apply(event)
    }

    override func mouseUp(with event: NSEvent) {
        guard isScrubbing else { return }
        isScrubbing = false
        apply(event, finished: true)
    }

    private func apply(_ event: NSEvent, finished: Bool = false) {
        let p = convert(event.locationInWindow, from: nil)
        value = min(1, max(0, p.x / max(1, bounds.width)))
        onScrub?(value, finished)
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

// MARK: - Player bar

final class QTSQAudioPlayerView: NSView {

    /// Local transport change → the embedder pushes it to the engine mirror
    /// (command 16: playing flag + position seconds).
    var onTransport: ((Bool, Float) -> Void)?

    private var player: AVAudioPlayer?
    private var currentURL = ""
    private var currentCAF = ""
    private var mediaDuration: Float = 0
    /// The last (playing, position) we sent to the engine; the next frame
    /// echoing it back must not be re-applied (echo suppression).
    private var pendingEcho: (playing: Bool, position: Float, at: Date)?

    private let playButton = GlyphButton(symbolName: "play.fill", glyphSize: 11) {}
    private let pauseButton = GlyphButton(symbolName: "pause.fill", glyphSize: 11) {}
    private let scrubber = MediaScrubber()
    private let elapsedLabel = NSTextField(labelWithString: "0:00")
    private let durationLabel = NSTextField(labelWithString: "0:00")
    private let speakerIcon = NSImageView()
    private var tick: Timer?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true

        if let base = NSImage(systemSymbolName: "speaker.wave.2.fill",
                              accessibilityDescription: "Audio")?
            .withSymbolConfiguration(.init(pointSize: 12, weight: .medium)) {
            speakerIcon.image = base
            speakerIcon.contentTintColor = Theme.chromeMutedText
        }

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

        addSubview(speakerIcon)
        addSubview(playButton)
        addSubview(pauseButton)
        addSubview(elapsedLabel)
        addSubview(scrubber)
        addSubview(durationLabel)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

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
        speakerIcon.frame = NSRect(x: 16, y: (h - 16) / 2, width: 16, height: 16)
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
        if media.url != currentURL || media.cafPath != currentCAF {
            load(cafrPath: media.cafPath, url: media.url, duration: media.duration)
        }
        guard let player else { return }

        // Echo suppression: while a local command is in flight, frames that
        // don't yet reflect it are pre-echo; the matching one confirms it.
        if let echo = pendingEcho {
            if echo.playing == media.playing, abs(echo.position - media.position) < 0.05 {
                pendingEcho = nil
                syncButtons()
                return
            }
            if Date().timeIntervalSince(echo.at) < 2 { return }
            pendingEcho = nil
        }

        if media.playing != player.isPlaying {
            // State flipped elsewhere (reload, history restore): apply it.
            if media.playing {
                player.currentTime = Double(media.position)
                player.play()
            } else {
                player.pause()
                player.currentTime = Double(media.position)
            }
        } else if !media.playing,
                  abs(media.position - Float(player.currentTime)) > 0.02 {
            // Both paused: the mirror's position is authoritative.
            player.currentTime = Double(media.position)
        }
        // Both playing: the local clock advances; the engine mirror records
        // only what we last sent, so its position is not a correction.
        syncButtons()
    }

    /// The bar is being hidden (navigation away, tab switch): stop playback
    /// honestly — tell the engine we paused so its mirror stays truthful.
    func detach() {
        tick?.invalidate()
        tick = nil
        if let player, player.isPlaying {
            player.pause()
            let pos = Float(player.currentTime)
            pendingEcho = (false, pos, Date())
            onTransport?(false, pos)
        }
        player = nil
        currentURL = ""
        currentCAF = ""
        syncButtons()
    }

    // MARK: Internals

    private func load(cafrPath: String, url: String, duration: Float) {
        tick?.invalidate()
        tick = nil
        currentURL = url
        currentCAF = cafrPath
        mediaDuration = duration
        pendingEcho = nil
        player = try? AVAudioPlayer(contentsOf: URL(fileURLWithPath: cafrPath))
        if mediaDuration <= 0 { mediaDuration = Float(player?.duration ?? 0) }
        scrubber.value = 0
        scrubber.enabled = player != nil
        playButton.isEnabled = player != nil
        refreshLabels()
        startTick()
    }

    private func togglePlay() {
        guard let player else { return }
        let nowPlaying = !player.isPlaying
        if nowPlaying { player.play() } else { player.pause() }
        let pos = Float(player.currentTime)
        pendingEcho = (nowPlaying, pos, Date())
        syncButtons()
        onTransport?(nowPlaying, pos)
    }

    private func scrubbed(to value: CGFloat, finished: Bool) {
        guard let player else { return }
        let target = Double(value) * Double(mediaDuration > 0 ? mediaDuration : Float(player.duration))
        player.currentTime = target
        refreshLabels()
        guard finished else { return }
        let pos = Float(target)
        pendingEcho = (player.isPlaying, pos, Date())
        onTransport?(player.isPlaying, pos)
    }

    private func startTick() {
        let t = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.tickFired()
        }
        RunLoop.main.add(t, forMode: .common)
        tick = t
    }

    private func tickFired() {
        guard let player, !scrubber.isScrubbing else { return }

        // Natural finish: AVAudioPlayer stops by itself, so the UI has to
        // notice — flip the buttons and tell the engine we're paused at 0
        // (replay starts from the top, like a normal transport).
        if playButton.isHidden, !player.isPlaying {
            player.currentTime = 0
            scrubber.value = 0
            refreshLabels()
            syncButtons()
            pendingEcho = (false, 0, Date())
            onTransport?(false, 0)
            return
        }

        guard player.isPlaying else { return }
        let duration = mediaDuration > 0 ? mediaDuration : Float(player.duration)
        scrubber.value = duration > 0
            ? CGFloat(player.currentTime / Double(duration)) : 0
        refreshLabels()
    }

    private func syncButtons() {
        let playing = player?.isPlaying ?? false
        playButton.isHidden = playing
        pauseButton.isHidden = !playing
    }

    private func refreshLabels() {
        guard let player else {
            elapsedLabel.stringValue = "0:00"
            durationLabel.stringValue = format(mediaDuration)
            return
        }
        elapsedLabel.stringValue = format(Float(player.currentTime))
        let duration = mediaDuration > 0 ? mediaDuration : Float(player.duration)
        durationLabel.stringValue = format(duration)
    }

    private func format(_ seconds: Float) -> String {
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
