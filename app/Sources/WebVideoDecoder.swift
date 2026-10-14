// WebVideoDecoder.swift — F2/F4: VideoToolbox decode for web `<video controls>`.
//
// The engine demuxes `<video>` sources into compressed samples (kitae_media)
// and paints + hit-tests the builtin transport chrome itself. The host's only
// web-video job is the pixel pipeline: this file holds the per-URL decoder
// session (H.264 via avcC, HEVC via hvcC, VP9 via a plain format description;
// VP8/AV1 have no VideoToolbox support and degrade to the placeholder) and
// the clock driver that advances the transport mirror while the element
// plays — decode `pos * fps`, feed RGBA back through `kitae_web_video_feed`,
// then push `mediaPlay(true, pos)` so the engine republishes with the new
// raster stamped (feed BEFORE play — the republish must see the raster).

import Foundation
import CoreMedia
import CoreVideo
import VideoToolbox

// ---------------------------------------------------------------------------
// Decoder: one VTDecompressionSession per media URL
// ---------------------------------------------------------------------------

final class WebVideoDecoder {
    /// Codec ids — kitae_core.h's v20 table: 1 H264, 2 HEVC, 3 VP8, 4 VP9,
    /// 5 AV1. VP8/AV1 return nil from init (no VideoToolbox decoder).
    private static let codecH264: UInt32 = 1
    private static let codecHevc: UInt32 = 2
    private static let codecVp9: UInt32 = 4

    /// The resolved media URL (the frame's media `src`) the C API keys on.
    let url: String
    let width: Int
    let height: Int
    /// Nominal frame rate (engine demux; 25 as a fallback so the clock and
    /// sample indexing never divide by zero).
    let fps: Double
    /// Compressed samples in the track — the decode clock's frame index bound.
    let sampleCount: Int
    /// Seconds of content: samples / fps (the v5 media section carries the
    /// same value; this is the driver's fallback).
    var sampleDuration: Double { Double(sampleCount) / max(fps, 1.0) }

    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?
    /// avcC/hvcC NALU length prefix size; 0 = raw payloads (VP9). Samples
    /// are re-prefixed to 4 bytes before decode when the container used
    /// something else — the format description is built with 4.
    private let nalLengthSize: Int

    init?(url: String) {
        let codec = url.withCString { kitae_web_video_codec($0) }
        guard codec == WebVideoDecoder.codecH264
                  || codec == WebVideoDecoder.codecHevc
                  || codec == WebVideoDecoder.codecVp9 else { return nil }
        var w: UInt32 = 0
        var h: UInt32 = 0
        guard url.withCString({ kitae_web_video_dims($0, &w, &h) }) == 1,
              w > 0, h > 0 else { return nil }
        var fps = url.withCString { kitae_web_video_fps($0) }
        if fps <= 0 { fps = 25 }
        let count = Int(url.withCString { kitae_web_video_sample_count($0) })
        guard count > 0 else { return nil }
        // Two-call sizing idiom: buf == NULL returns the full length.
        let exLen = Int(url.withCString { kitae_web_video_extradata($0, nil, 0) })
        var extradata: [UInt8] = []
        if exLen > 0 {
            extradata = [UInt8](repeating: 0, count: exLen)
            _ = url.withCString { kitae_web_video_extradata($0, &extradata, UInt(exLen)) }
        }

        self.url = url
        self.width = Int(w)
        self.height = Int(h)
        self.fps = fps
        self.sampleCount = count
        self.session = nil

        switch codec {
        case WebVideoDecoder.codecH264:
            guard let (sets, lenSize) = WebVideoDecoder.h264Sets(from: extradata),
                  let f = WebVideoDecoder.formatFromParameterSets(sets,
                                                                  creator: {
                      var out: CMVideoFormatDescription?
                      CMVideoFormatDescriptionCreateFromH264ParameterSets(
                          allocator: kCFAllocatorDefault,
                          parameterSetCount: $0, parameterSetPointers: $1,
                          parameterSetSizes: $2, nalUnitHeaderLength: 4,
                          formatDescriptionOut: &out)
                      return out
                  }) else { return nil }
            format = f
            nalLengthSize = lenSize
        case WebVideoDecoder.codecHevc:
            guard let sets = WebVideoDecoder.hevcSets(from: extradata),
                  let f = WebVideoDecoder.formatFromParameterSets(sets,
                                                                  creator: {
                      var out: CMVideoFormatDescription?
                      CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                          allocator: kCFAllocatorDefault,
                          parameterSetCount: $0, parameterSetPointers: $1,
                          parameterSetSizes: $2, nalUnitHeaderLength: 4,
                          extensions: nil, formatDescriptionOut: &out)
                      return out
                  }) else { return nil }
            format = f
            nalLengthSize = 4
        default: // VP9: raw frame payloads + a plain (vpcC-extended) description
            guard let f = WebVideoDecoder.formatVp9(width: Int(w), height: Int(h),
                                                    extradata: extradata) else { return nil }
            format = f
            nalLengthSize = 0
        }
        guard let desc = format,
              let s = WebVideoDecoder.createSession(format: desc,
                                                    width: Int(w), height: Int(h),
                                                    owner: self) else { return nil }
        session = s
    }

    deinit {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
    }

    /// Decode sample `index` to RGBA8 (straight alpha). Blocks until the
    /// decompression callback delivers the pixel buffer (hardware decode is
    /// ~ms for web-clip sizes); nil when the sample misses or decode fails.
    func decodeFrame(_ index: Int) -> [UInt8]? {
        guard let session, index >= 0, index < sampleCount else { return nil }
        let i = UInt32(index)
        let len = Int(url.withCString { kitae_web_video_sample($0, i, nil, 0) })
        guard len > 0 else { return nil }
        var raw = [UInt8](repeating: 0, count: len)
        _ = url.withCString { kitae_web_video_sample($0, i, &raw, UInt(len)) }

        var payload: [UInt8]
        switch nalLengthSize {
        case 4: payload = raw
        case 0: payload = raw // VP9: raw frame payload straight through
        default: payload = WebVideoDecoder.fourBytePrefixes(raw, from: nalLengthSize)
        }

        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
                  allocator: nil, memoryBlock: nil, blockLength: payload.count,
                  blockAllocator: nil, customBlockSource: nil, offsetToData: 0,
                  dataLength: payload.count, flags: 0, blockBufferOut: &block)
                  == kCMBlockBufferNoErr,
              let block else { return nil }
        let filled = payload.withUnsafeBytes { bytes -> Bool in
            CMBlockBufferReplaceDataBytes(with: bytes.baseAddress!, blockBuffer: block,
                                          offsetIntoDestination: 0,
                                          dataLength: payload.count) == kCMBlockBufferNoErr
        }
        guard filled else { return nil }
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReady(allocator: nil, dataBuffer: block,
                                        formatDescription: format, sampleCount: 1,
                                        sampleTimingEntryCount: 0, sampleTimingArray: nil,
                                        sampleSizeEntryCount: 0, sampleSizeArray: nil,
                                        sampleBufferOut: &sample) == noErr,
              let sample else { return nil }

        let request = DecodeRequest()
        let opaque = Unmanaged.passUnretained(request).toOpaque()
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample, flags: [],
            frameRefcon: opaque, infoFlagsOut: nil)
        guard status == noErr else { return nil }
        // Synchronous decode fires the callback before returning; the wait
        // is a safety net against decoders that defer the delivery.
        guard request.semaphore.wait(timeout: .now() + 2.0) == .success else { return nil }
        return request.pixels
    }

    private final class DecodeRequest {
        let semaphore = DispatchSemaphore(value: 0)
        var pixels: [UInt8]?
    }

    /// The session's output callback: BGRA pixel buffer in, RGBA bytes out
    /// (bytes 0 and 2 swap), then the waiting decode unblocks.
    private static func output(_ sourceRefCon: UnsafeMutableRawPointer?,
                               _ status: OSStatus, _ imageBuffer: CVImageBuffer?) {
        guard let sourceRefCon else { return }
        let request = Unmanaged<DecodeRequest>.fromOpaque(sourceRefCon).takeUnretainedValue()
        defer { request.semaphore.signal() }
        guard status == noErr, let pb = imageBuffer else { return }
        guard CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return }
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb) else { return }
        let w = CVPixelBufferGetWidth(pb)
        let h = CVPixelBufferGetHeight(pb)
        let bpr = CVPixelBufferGetBytesPerRow(pb)
        var rgba = [UInt8](repeating: 0, count: w * h * 4)
        rgba.withUnsafeMutableBufferPointer { dst in
            for row in 0..<h {
                let src = base.advanced(by: row * bpr).assumingMemoryBound(to: UInt8.self)
                let d = dst.baseAddress! + row * w * 4
                var x = 0
                while x < w * 4 {
                    d[x] = src[x + 2]     // B -> R
                    d[x + 1] = src[x + 1] // G
                    d[x + 2] = src[x]     // R -> B
                    d[x + 3] = src[x + 3] // A
                    x += 4
                }
            }
        }
        request.pixels = rgba
    }

    private static func createSession(format: CMVideoFormatDescription,
                                      width: Int, height: Int,
                                      owner: WebVideoDecoder) -> VTDecompressionSession? {
        let dest: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String:
                NSNumber(value: kCVPixelFormatType_32BGRA),
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
        ]
        var record = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: { _, sourceRefCon, status, _, imageBuffer, _, _ in
                WebVideoDecoder.output(sourceRefCon, status, imageBuffer)
            },
            decompressionOutputRefCon: Unmanaged.passUnretained(owner).toOpaque())
        var session: VTDecompressionSession?
        guard VTDecompressionSessionCreate(
                  allocator: nil, formatDescription: format,
                  decoderSpecification: nil,
                  imageBufferAttributes: dest as CFDictionary,
                  outputCallback: &record,
                  decompressionSessionOut: &session) == noErr else { return nil }
        return session
    }

    // ── container parsing ──────────────────────────────────────────────────

    /// avcC → (SPS..., PPS..., NALU length size). Layout: version, profile,
    /// compat, level, [4] reserved+lengthSizeMinusOne, [5] reserved+numSPS,
    /// SPS {u16 len + bytes}, numPPS, PPS {u16 len + bytes}.
    private static func h264Sets(from avcC: [UInt8]) -> (sets: [[UInt8]], lenSize: Int)? {
        guard avcC.count > 6 else { return nil }
        let lenSize = Int(avcC[4] & 0x03) + 1
        var p = 5
        var sets: [[UInt8]] = []
        let numSPS = Int(avcC[5] & 0x1F)
        for _ in 0..<numSPS {
            guard p + 2 <= avcC.count else { return nil }
            let n = Int(avcC[p]) << 8 | Int(avcC[p + 1])
            p += 2
            guard n > 0, p + n <= avcC.count else { return nil }
            sets.append(Array(avcC[p..<p + n]))
            p += n
        }
        guard p < avcC.count else { return nil }
        let numPPS = Int(avcC[p])
        p += 1
        for _ in 0..<numPPS {
            guard p + 2 <= avcC.count else { return nil }
            let n = Int(avcC[p]) << 8 | Int(avcC[p + 1])
            p += 2
            guard n > 0, p + n <= avcC.count else { return nil }
            sets.append(Array(avcC[p..<p + n]))
            p += n
        }
        guard sets.count == numSPS + numPPS else { return nil }
        return (sets, lenSize)
    }

    /// hvcC → (VPS..., SPS..., PPS...). The 23-byte header ends with
    /// numOfArrays; each array is [1 type byte][2 numNalus] then
    /// NAL units {u16 len + bytes}. We keep NAL types 32/33/34.
    private static func hevcSets(from hvcC: [UInt8]) -> [[UInt8]]? {
        guard hvcC.count > 23 else { return nil }
        var p = 22
        let numArrays = Int(hvcC[p])
        p += 1
        var vps: [[UInt8]] = []
        var sps: [[UInt8]] = []
        var pps: [[UInt8]] = []
        for _ in 0..<numArrays {
            guard p + 3 <= hvcC.count else { return nil }
            let nalType = hvcC[p] & 0x3F
            let num = Int(hvcC[p + 1]) << 8 | Int(hvcC[p + 2])
            p += 3
            for _ in 0..<num {
                guard p + 2 <= hvcC.count else { return nil }
                let n = Int(hvcC[p]) << 8 | Int(hvcC[p + 1])
                p += 2
                guard n > 0, p + n <= hvcC.count else { return nil }
                let nalu = Array(hvcC[p..<p + n])
                p += n
                switch nalType {
                case 32: vps.append(nalu)
                case 33: sps.append(nalu)
                case 34: pps.append(nalu)
                default: break
                }
            }
        }
        guard !vps.isEmpty, !sps.isEmpty, !pps.isEmpty else { return nil }
        return vps + sps + pps
    }

    /// Build a format description from raw parameter-set NALUs. The sets are
    /// flattened into one buffer so the pointers stay alive for the call;
    /// `creator` picks the H.264 vs HEVC entry point.
    private static func formatFromParameterSets(
        _ sets: [[UInt8]],
        creator: (Int, UnsafePointer<UnsafePointer<UInt8>>, UnsafePointer<Int>)
            -> CMVideoFormatDescription?
    ) -> CMVideoFormatDescription? {
        guard !sets.isEmpty else { return nil }
        var flat: [UInt8] = []
        var spans: [(offset: Int, len: Int)] = []
        for s in sets {
            spans.append((flat.count, s.count))
            flat += s
        }
        return flat.withUnsafeBufferPointer { buf -> CMVideoFormatDescription? in
            guard let base = buf.baseAddress else { return nil }
            var ptrs: [UnsafePointer<UInt8>] = spans.map { base + $0.offset }
            var sizes: [Int] = spans.map { $0.len }
            return creator(sets.count, &ptrs, &sizes)
        }
    }

    /// VP9: the frame payloads are self-describing, so a plain description
    /// usually suffices; attach the vpcC atom when the container carried one
    /// (WebM CodecPrivate — either the raw payload or the full box).
    private static func formatVp9(width: Int, height: Int,
                                  extradata: [UInt8]) -> CMVideoFormatDescription? {
        var vpcC: Data?
        if extradata.count > 8,
           Array(extradata[4..<8]) == Array("vpcC".utf8) {
            vpcC = Data(extradata[8...]) // full box: strip the 8-byte header
        } else if extradata.count >= 19 {
            vpcC = Data(extradata)       // raw vpcC payload
        }
        let extensions: CFDictionary?
        if let vpcC {
            let atoms = ["vpcC": vpcC] as CFDictionary
            extensions = [kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: atoms]
                as CFDictionary
        } else {
            extensions = nil
        }
        var out: CMVideoFormatDescription?
        CMVideoFormatDescriptionCreate(allocator: kCFAllocatorDefault,
                                       codecType: kCMVideoCodecType_VP9,
                                       width: Int32(width), height: Int32(height),
                                       extensions: extensions, formatDescriptionOut: &out)
        return out
    }

    /// Re-prefix AVCC NALUs with 4-byte lengths when the container's
    /// lengthSizeMinusOne said something else (VideoToolbox expects 4).
    private static func fourBytePrefixes(_ raw: [UInt8], from lenSize: Int) -> [UInt8] {
        var out: [UInt8] = []
        out.reserveCapacity(raw.count)
        var p = 0
        while p + lenSize <= raw.count {
            var n = 0
            for i in 0..<lenSize { n = (n << 8) | Int(raw[p + i]) }
            p += lenSize
            guard n > 0, p + n <= raw.count else { break }
            out.append(contentsOf: [0, 0, UInt8(n >> 8), UInt8(n & 0xFF)])
            out.append(contentsOf: raw[p..<p + n])
            p += n
        }
        return out
    }
}

// ---------------------------------------------------------------------------
// Driver: the web video's clock lives here, not in the engine
// ---------------------------------------------------------------------------

/// Follows every frame each tab produces; when the page carries a
/// `<video controls>` element, owns its transport: primes frame 0, runs the
/// wall-clock tick while playing (decode → feed → `mediaPlay` so the engine
/// chrome advances), and decodes single frames on paused seeks. Echo
/// suppression keeps our own pushed mirror from re-triggering the state
/// machine (same pattern as the QTSQ docked bars).
final class WebVideoDriver {
    static let shared = WebVideoDriver()

    /// The page's first controls video drives the transport — the engine's
    /// media session is first-wins too (documented limitation), so extra
    /// items show their primed frame only.
    private final class Slot {
        weak var tab: Tab?
        var url = ""
        var decoder: WebVideoDecoder?
        var primed = false
        var playing = false
        var anchor = (position: Float(0), at: Date())
        var lastFedIndex = -1
        var lastFedPosition = Float(-1)
        var pendingEcho: (playing: Bool, position: Float, at: Date)?
        var timer: Timer?
    }

    private var slots: [ObjectIdentifier: Slot] = [:]

    private func slot(for tab: Tab) -> Slot {
        let key = ObjectIdentifier(tab)
        if let existing = slots[key] { return existing }
        let slot = Slot()
        slot.tab = tab
        slots[key] = slot
        return slot
    }

    /// A frame arrived on `tab`'s engine. Cheap on every frame: one
    /// first(where:) + the echo compare when nothing changed.
    func observe(tab: Tab, frame: EngineFrame) {
        let slot = slot(for: tab)
        guard let item = frame.mediaItems.first(where: { $0.controls && $0.kind == .video }) else {
            stop(tab: tab)
            return
        }
        if slot.url != item.src {
            slot.url = item.src
            slot.decoder = WebVideoDecoder(url: item.src)
            slot.primed = false
            slot.playing = false
            slot.lastFedIndex = -1
            slot.lastFedPosition = -1
            slot.pendingEcho = nil
            stopTimer(slot)
        }
        guard let decoder = slot.decoder else { return } // unsupported codec: placeholder stays

        // Our own pushes reflect back through the transport mirror — consume
        // them so the state machine only reacts to external events.
        if let echo = slot.pendingEcho,
           Date().timeIntervalSince(echo.at) < 2.0,
           echo.playing == item.playing,
           abs(echo.position - item.position) < 0.05 {
            slot.pendingEcho = nil
            return
        }
        slot.pendingEcho = nil

        if !slot.primed {
            // First sight: decode + feed frame 0, then nudge a republish so
            // the primed raster stamps into the page image.
            slot.primed = true
            feed(slot, decoder, index: 0)
            tab.engine.mediaPlay(playing: item.playing, position: item.position)
            slot.pendingEcho = (item.playing, item.position, Date())
            slot.anchor = (item.position, Date())
            slot.playing = item.playing
            if item.playing { startTimer(slot) }
            return
        }

        if item.playing {
            if slot.playing {
                // Chrome seek while playing: the mirror jumped off our clock.
                let expected = slot.anchor.position
                    + Float(Date().timeIntervalSince(slot.anchor.at))
                if abs(item.position - expected) > 0.25 {
                    slot.anchor = (item.position, Date())
                    feedAt(slot, decoder, position: item.position)
                    tab.engine.mediaPlay(playing: true, position: item.position)
                    slot.pendingEcho = (true, item.position, Date())
                }
            } else {
                // Chrome play click: the mirror flipped to playing — take over.
                slot.playing = true
                slot.anchor = (item.position, Date())
                feedAt(slot, decoder, position: item.position)
                tab.engine.mediaPlay(playing: true, position: item.position)
                slot.pendingEcho = (true, item.position, Date())
                startTimer(slot)
            }
        } else {
            if slot.playing {
                // Chrome pause click (or end-of-stream): stop the clock.
                slot.playing = false
                stopTimer(slot)
                feedAt(slot, decoder, position: item.position)
                tab.engine.mediaPlay(playing: false, position: item.position)
                slot.pendingEcho = (false, item.position, Date())
            } else if abs(item.position - slot.lastFedPosition) > 0.02 {
                // Paused seek: chrome scrubber click. Decode + stamp once.
                feedAt(slot, decoder, position: item.position)
                tab.engine.mediaPlay(playing: false, position: item.position)
                slot.pendingEcho = (false, item.position, Date())
            }
        }
    }

    /// The tab went away (closed, or its page lost the video): drop the slot.
    func stop(tab: Tab) {
        let key = ObjectIdentifier(tab)
        guard let slot = slots.removeValue(forKey: key) else { return }
        slot.timer?.invalidate()
        slot.timer = nil
        slot.tab = nil
    }

    func clearAll() {
        let current = slots
        slots.removeAll()
        for slot in current.values {
            stopTimer(slot)
            slot.decoder = nil
            slot.tab = nil
        }
    }

    // MARK: The playing clock

    private func startTimer(_ slot: Slot) {
        guard slot.timer == nil else { return }
        let timer = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            guard let self, let slot = self.liveSlot(slot) else { return }
            self.tick(slot)
        }
        RunLoop.main.add(timer, forMode: .common)
        slot.timer = timer
    }

    private func stopTimer(_ slot: Slot) {
        slot.timer?.invalidate()
        slot.timer = nil
    }

    /// The tick: advance the wall-clock position, decode the sample under
    /// it, feed the raster, then mirror the transport — feed BEFORE play so
    /// the engine's republish stamps the frame the chrome now points at.
    private func tick(_ slot: Slot) {
        guard let tab = slot.tab, let decoder = slot.decoder,
              let frame = tab.latestFrame,
              frame.mediaItems.contains(where: { $0.controls && $0.kind == .video })
        else {
            stopTimer(slot)
            return
        }
        let now = Date()
        let pos = slot.anchor.position + Float(now.timeIntervalSince(slot.anchor.at))
        let reportedDuration = frame.media?.duration ?? 0
        let duration = reportedDuration > 0 ? reportedDuration : Float(decoder.sampleDuration)
        if pos >= duration && duration > 0 {
            // End of stream: land on the last frame, park the transport.
            feedAt(slot, decoder, position: duration)
            slot.playing = false
            stopTimer(slot)
            tab.engine.mediaPlay(playing: false, position: duration)
            slot.pendingEcho = (false, duration, now)
            return
        }
        slot.anchor = (pos, now)
        feedAt(slot, decoder, position: pos)
        tab.engine.mediaPlay(playing: true, position: pos)
        slot.pendingEcho = (true, pos, now)
    }

    private func feedAt(_ slot: Slot, _ decoder: WebVideoDecoder, position: Float) {
        let idx = min(max(Int(position * Float(decoder.fps)), 0), decoder.sampleCount - 1)
        slot.lastFedPosition = position
        guard idx != slot.lastFedIndex else { return }
        slot.lastFedIndex = idx
        feed(slot, decoder, index: idx)
    }

    private func feed(_ slot: Slot, _ decoder: WebVideoDecoder, index: Int) {
        guard let pixels = decoder.decodeFrame(index) else { return }
        decoder.url.withCString { cUrl in
            pixels.withUnsafeBufferPointer { buf in
                guard let base = buf.baseAddress else { return }
                _ = kitae_web_video_feed(cUrl, UInt32(decoder.width),
                                         UInt32(decoder.height), base,
                                         UInt(pixels.count))
            }
        }
    }

    /// The slot still lives in the driver's dict (its tab wasn't closed).
    private func liveSlot(_ slot: Slot) -> Slot? {
        slots.values.first { $0 === slot }
    }
}
