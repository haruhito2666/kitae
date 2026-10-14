// EngineFrame.swift — the Swift-side decoder for the engine's serialized
// frame (wire format documented in kitae_core.h and produced by
// crates/kitae_core/src/ffi.rs).
//
// A frame is a complete repaint: chrome flags, page metrics, and the display
// list (rects → texts → links → images), all in page space. Decoding is
// pure — the controller hands the result to the canvas on the main thread.

import AppKit
import Foundation

struct PageRect {
    var x: Float, y: Float, w: Float, h: Float
    var color: UInt32  // 0xRRGGBBAA (fast-path solid approximation)
    /// Per-corner radii in px (v7+; zero on older frames).
    var radius: (Float, Float, Float, Float) = (0, 0, 0, 0)
    /// Accurate fill (v7+; `.solid` on older frames).
    var paint: PagePaint = .solid
    /// Affine warp of this item (v12+; nil = identity). Page-space, same
    /// convention as the layout crate.
    var transform: PageTransform?
}

/// Q2: 2×3 row-major affine matrix (x' = m0·x + m1·y + m2; y' = m3·x +
/// m4·y + m5), stored page-space on the item it warps.
struct PageTransform {
    var m0: Float, m1: Float, m2: Float
    var m3: Float, m4: Float, m5: Float
}

/// Accurate rect fill (v7+).
enum PagePaint {
    case solid
    case linear(angle: Float, stops: [(offset: Float, color: UInt32)])
    case radial(cx: Float, cy: Float, radius: Float?, stops: [(offset: Float, color: UInt32)])
    case conic(angle: Float, cx: Float, cy: Float, stops: [(offset: Float, color: UInt32)])
    case image(url: String)
}

/// Signature verdict for signed QTSQ payloads (v11 frame byte), mirroring
/// kitae_core's `QtsqSigState`.
enum QtsqSigState {
    case unsigned
    case valid
    case invalid
    case unverified
}

/// One box shadow (v7+): border-box geometry plus paint-time expansion.
struct PageShadow {
    var x: Float, y: Float, w: Float, h: Float
    var radius: (Float, Float, Float, Float)
    var color: UInt32
    var blur: Float
    var spread: Float
    var offsetX: Float
    var offsetY: Float
    var inset: Bool
    var transform: PageTransform?
}

/// One find-in-page match (v8+): page-space highlight boxes, one per text
/// run the match touches, in page order.
struct FindMatch {
    var rects: [(x: Float, y: Float, w: Float, h: Float)]
}

/// One accessibility node (v8+): role, human label and page-space bounds
/// (zero rect = unresolved). Children are indices into the frame's flat
/// `axNodes` list.
enum AxChecked: UInt8 {
    case unspecified = 0
    case unchecked = 1
    case checked = 2
    case mixed = 3
}

enum AxLivePoliteness: UInt8 {
    case off = 0
    case polite = 1
    case assertive = 2
}

struct AxNode {
    var id: String
    /// Wire role id (0 document … 10 group, 11 live region; see kitae_core.h).
    var role: UInt8
    var label: String
    var x: Float, y: Float, w: Float, h: Float
    var children: [Int]
    var value: String?
    var valueMin: Float?
    var valueMax: Float?
    var disabled = false
    var checked: AxChecked = .unspecified
    var expanded: Bool?
    var selected: Bool?
    var headingLevel: UInt8 = 0
    var livePoliteness: AxLivePoliteness = .off
}

struct PageText {
    var x: Float
    /// Baseline y.
    var y: Float
    var size: Float
    var weight: UInt32
    var italic: Bool
    var monospace: Bool
    var color: UInt32
    /// Extra px after each glyph except the last (v10+, Q3). 0 = draw the
    /// whole run as one line; the engine measured this run char-by-char.
    var letterSpacing: Float = 0
    /// First non-generic `font-family` (v14+, Q11). The measurer resolved
    /// widths with this family (empty = the host's default), so the painter
    /// must resolve the same one or glyphs will not match the layout.
    var family: String = ""
    /// Blur radius of the text-shadow this duplicate run paints (v19 tail,
    /// E3; 0 = crisp run). The run's color already carries the shadow color;
    /// the painter blurs the alpha it draws (Core Text via NSShadow).
    var shadowBlur: Float = 0
    /// Affine warp of this run (v12+; nil = identity).
    var transform: PageTransform?
    var text: String
}

struct PageLink {
    var x: Float, y: Float, w: Float, h: Float
    var href: String
    var transform: PageTransform? = nil
}

extension PageTransform {
    func applying(x: Float, y: Float) -> (x: Float, y: Float) {
        (m0 * x + m1 * y + m2, m3 * x + m4 * y + m5)
    }

    func inverted() -> PageTransform? {
        let determinant = m0 * m4 - m1 * m3
        guard determinant.isFinite, determinant != 0 else { return nil }
        let inverse = PageTransform(
            m0: m4 / determinant,
            m1: -m1 / determinant,
            m2: (m1 * m5 - m4 * m2) / determinant,
            m3: -m3 / determinant,
            m4: m0 / determinant,
            m5: (m3 * m2 - m0 * m5) / determinant
        )
        return [inverse.m0, inverse.m1, inverse.m2, inverse.m3, inverse.m4, inverse.m5]
            .allSatisfy { $0.isFinite } ? inverse : nil
    }
}

extension PageLink {
    func contains(x: Float, y: Float) -> Bool {
        let local: (x: Float, y: Float)
        if let transform {
            guard let inverse = transform.inverted() else { return false }
            local = inverse.applying(x: x, y: y)
        } else {
            local = (x, y)
        }
        return local.x >= self.x - 0.001 && local.x <= self.x + w + 0.001
            && local.y >= self.y - 0.001 && local.y <= self.y + h + 0.001
    }

    var pageBounds: NSRect {
        let corners = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)]
        let points = transform.map {
            transform in corners.map { transform.applying(x: $0.0, y: $0.1) }
        } ?? corners
        let minX = points.reduce(Float.infinity) { min($0, $1.0) }
        let maxX = points.reduce(-Float.infinity) { max($0, $1.0) }
        let minY = points.reduce(Float.infinity) { min($0, $1.1) }
        let maxY = points.reduce(-Float.infinity) { max($0, $1.1) }
        return NSRect(x: CGFloat(minX), y: CGFloat(minY),
                      width: CGFloat(maxX - minX), height: CGFloat(maxY - minY))
    }
}

/// One CSS filter function (v16+, B4). Mirrors kitae_css `FilterFn`:
/// scalars carry one argument; drop-shadow carries offsets, blur radius
/// and an RGBA color (0 when the CSS said `currentColor`/omitted).
enum PageFilterFn {
    case blur(Float)
    case dropShadow(dx: Float, dy: Float, blur: Float, color: UInt32)
    case brightness(Float)
    case contrast(Float)
    case grayscale(Float)
    case hueRotate(Float)
    case invert(Float)
    case opacity(Float)
    case saturate(Float)
    case sepia(Float)
}

/// A filter chain attached to one painted item (v16+). `kind` selects the
/// list: 0 rects, 1 texts, 2 images, 3 shadows — same ordering as the
/// engine's `ffi.rs` filter tail; `index` is bounds-checked at use.
struct PageFilterEntry {
    var kind: Int
    var index: Int
    var fns: [PageFilterFn]
}

/// A scrollable region the engine's layout produced (v17+, B2/B3): the
/// styled node's content overflowed its `overflow: auto|scroll` box.
/// Page-space rect, content extents, mode, and nearest scroll parent.
struct PageScrollContainer {
    var node: UInt32
    var x: Float
    var y: Float
    var w: Float
    var h: Float
    var contentWidth: Float
    var contentHeight: Float
    var mode: UInt8
    var parent: UInt32
}

/// A resolved clip-path/mask shape (v18+, B7) in page space. Mirrors
/// kitae_layout `ResolvedClip`; the tags on the wire (circle 1, ellipse 2,
/// inset 3, polygon 4) map 1:1 onto these cases. Inset radii are elliptical
/// (horizontal, vertical) pairs in rect order tl, tr, br, bl.
enum PageClip {
    case circle(cx: Float, cy: Float, r: Float)
    case ellipse(cx: Float, cy: Float, rx: Float, ry: Float)
    case inset(x: Float, y: Float, w: Float, h: Float, radii: [(Float, Float)])
    case polygon(evenOdd: Bool, points: [(Float, Float)])
}

/// A clip/blend stamp attached to one painted item (v18+). `kind` selects
/// the list (0 rects, 1 texts, 2 images, 3 shadows — same ordering as the
/// filter tail); `index` is bounds-checked at use. B8 fills `blend`
/// (mix-blend-mode wire id, 0 = normal) and `group` (nonzero = paint
/// inside the host's offscreen layer for that `isolation: isolate` id);
/// `clip` is nil for blend/group-only entries.
struct PageClipEntry {
    var kind: Int
    var index: Int
    var clip: PageClip?
    var blend: UInt8
    var group: UInt32
}

/// One `position: sticky` box (v15+, Q5). `start`/`count` pairs index into
/// the frame's item lists for everything the box painted; the painter
/// shifts those items by the stick delta. All page-space values.
struct PageSticky {
    var rect: (UInt32, UInt32)
    var text: (UInt32, UInt32)
    var link: (UInt32, UInt32)
    var image: (UInt32, UInt32)
    var media: (UInt32, UInt32)
    var shadow: (UInt32, UInt32)
    /// Flow-position top of the box and its painted height.
    var baseY: Float
    var height: Float
    /// Containment bounds of the nearest scroll ancestor (or the document).
    var containerTop: Float
    var containerBottom: Float
    /// `top` / `bottom` offsets; `hasTop` picks the stick edge.
    var topOffset: Float
    var bottomOffset: Float
    var hasTop: Bool
    var hasBottom: Bool

    /// Y shift (page space) at `scrollY` with a `viewportHeight`-tall
    /// viewport — mirrors kitae_layout's `StickyItem::shift`.
    func shift(scrollY: Float, viewportHeight: Float) -> Float {
        if hasTop {
            let target = scrollY + topOffset - baseY
            let downMax = max(containerBottom - bottomOffset - height - baseY, 0)
            return min(max(target, 0), downMax)
        }
        if hasBottom {
            let target = scrollY + viewportHeight - bottomOffset - height - baseY
            let upMin = min(containerTop + topOffset - baseY, 0)
            return min(max(target, upMin), 0)
        }
        return 0
    }
}

struct PageImage {
    var x: Float, y: Float, w: Float, h: Float
    /// object-fit: 0 fill, 1 contain, 2 cover, 3 none, 4 scale-down.
    var fit: UInt32
    var src: String
    var alt: String
    /// Decoded-raster reference (v4+). 0 = draw the placeholder box; any
    /// other value resolves through kitae_qtsq_raster (engine-side cache).
    var payload: UInt64
    /// The raster is a multi-frame animation (v13+): step frames on our own
    /// clock via kitae_raster_frames/kitae_raster_frame.
    var animated: Bool
    /// Affine warp of this item (v12+; nil = identity).
    var transform: PageTransform?
}

struct EngineFrame {
    var canGoBack = false
    var canGoForward = false
    /// The engine laid this page out for print media (v20 `flags` bit2, set
    /// by `KITAE_CMD_SET_MEDIA`): `@media print` applied, `@media screen` did
    /// not. H3 — the print path waits for a frame with this set, hands it to
    /// `NSPrintOperation`, then asks for the screen frame back.
    var isPrintMedia = false
    var scrollY: Float = 0
    var contentHeight: Float = 0
    /// 0xRRGGBBAA page background; alpha 0 means "use the theme canvas".
    var pageBackground: UInt32 = 0
    var url = ""
    var title = ""
    var rects: [PageRect] = []
    var texts: [PageText] = []
    var links: [PageLink] = []
    var images: [PageImage] = []
    var focusedId = ""
    var selection = ""
    /// Media on the page (v5+): QTSQ audio/video viewer pages publish it;
    /// nil = no media. The engine decodes; the embedder plays.
    var media: MediaInfo?
    /// `<video>` elements on the page (v6+): rect + transport per element.
    /// The element's current frame paints through the image entry with the
    /// matching rect and a raster payload (engine-side stepped frame).
    var mediaItems: [MediaItem] = []
    /// Box shadows in paint order, parents before children (v7+).
    var shadows: [PageShadow] = []
    /// Find-in-page matches in page order (v8+); the embedder overlays the
    /// rects (dim for matches, strong for `findActive`).
    var findMatches: [FindMatch] = []
    /// Active match index (v8+); nil = no query or no matches.
    var findActive: Int?
    /// Accessibility tree, pre-order flattened (v8+); roots have no parent.
    /// Children are indices into this list (rebuilt on decode).
    var axNodes: [AxNode] = []
    /// Parent index per node (nil = root); parallel to `axNodes`.
    var axParents: [Int?] = []
    /// AX id of the focused node (v8+); "" = unfocused/unknown.
    var axFocusedId = ""
    /// The page is a QTSQ that failed to decode because it is encrypted
    /// (v9+) — the host offers a password prompt and replies with
    /// KITAE_CMD_SET_PASSWORD.
    var qtsqLocked = false
    /// Signature verdict for signed QTSQ payloads (v11+). Mirrors the
    /// engine's `QtsqSigState` ordering.
    var qtsqSig: QtsqSigState = .unsigned
    /// `position: sticky` boxes (v15+, Q5) in page order. Layout paints each
    /// box at its flow position; the painter shifts the box's items by
    /// `PageSticky.shift(scrollY:viewportHeight:)` — the engine applies the
    /// same formula to link hit-testing.
    var stickies: [PageSticky] = []

    /// Filter chains per painted item (v16+, B4). Sparse — see
    /// `PageFilterEntry`. Applied by the host at paint time.
    var filters: [PageFilterEntry] = []

    /// Scrollable regions (v17+, B2). The engine records where an
    /// `overflow: auto|scroll` box's content overflowed; the host paints
    /// the scrollbar and routes wheel events.
    var scrollContainers: [PageScrollContainer] = []

    /// Clip-path/mask shapes per painted item (v18+, B7). Sparse — see
    /// `PageClipEntry`. The host installs a CGPath clip at paint time.
    var clips: [PageClipEntry] = []

    /// Media attached to the current page (v5 wire section).
    struct MediaInfo {
        enum Kind { case audio, video }
        var kind: Kind
        var duration: Float
        /// Sample rate / channels (audio; 0 for video).
        var rate: UInt32
        var channels: UInt8
        /// Transport mirror from the engine.
        var playing: Bool
        var position: Float
        /// Absolute media document URL.
        var url: String
        /// Temp CAF path for audio ("" for video) — open with AVAudioPlayer.
        var cafPath: String
    }

    /// One `<video>` element (v6 wire section).
    struct MediaItem {
        enum Kind { case audio, video }
        var id: UInt64
        var x: Float, y: Float, w: Float, h: Float
        var src: String
        var kind: Kind
        var playing: Bool
        var position: Float
        /// The element carries `controls` (v20+): the engine demuxes it into
        /// the web-media store and the host paints + drives the builtin
        /// transport chrome band (decode loop via the web-video C API).
        var controls: Bool = false
    }

    /// Frame format version this decoder writes; it reads v1 (no images),
    /// v2 (images, no focus/selection), v3 (full), v4 (+ image payload ids),
    /// v5 (+ media section), v6 (+ media items), v7 (+ rect radius/paint,
    /// shadows), v8 (+ find-in-page, AX tree), v9 (+ qtsq locked flag),
    /// v10 (+ per-run letter spacing, Q3), v11 (+ qtsq signature verdict
    /// byte), v12 (+ sparse item transform matrices, Q2), v13 (+ image
    /// animated flag, Q8), v14 (+ per-run font family, Q11), v15 (+ sticky
    /// side-list, Q5), v16 (+ per-item filter chains, B4), v17 (+ scroll
    /// containers, B2), v18 (+ per-item clip-path/mask with a blend
    /// slot for B8, B7), v19 (+ sparse per-text shadow blur, E3) and
    /// v20 (+ per-media-item controls flag, F4) for compatibility. B3
    /// scroll metadata and AX semantics use optional sparse tails after v20.
    static let version = UInt32(KITAE_FRAME_VERSION)
    private static let axSemanticsMagic: UInt32 = 0x3153_5841

    /// Decode a little-endian frame buffer; nil on any malformed input.
    static func decode(_ data: [UInt8]) -> EngineFrame? {
        var reader = Reader(data)
        let ver = reader.u32()
        guard ver >= 1 && ver <= 20
        else { return nil }

        var f = EngineFrame()
        let flags = reader.u32()
        f.canGoBack = flags & 1 != 0
        f.canGoForward = flags & 2 != 0
        f.isPrintMedia = flags & 4 != 0
        f.scrollY = reader.f32()
        f.contentHeight = reader.f32()
        f.pageBackground = reader.u32()
        f.url = reader.string() ?? ""
        f.title = reader.string() ?? ""

        let rectCount = reader.u32()
        let textCount = reader.u32()
        let linkCount = reader.u32()
        let imageCount: UInt32 = (ver >= 2) ? reader.u32() : 0

        // Sanity: refuse absurd counts so a corrupt stream can't balloon.
        guard rectCount <= 1_000_000, textCount <= 1_000_000, linkCount <= 1_000_000,
              imageCount <= 1_000_000
        else { return nil }

        f.rects.reserveCapacity(Int(rectCount))
        for _ in 0..<rectCount {
            let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
            let color = reader.u32()
            guard reader.ok else { return nil }
            var radius: (Float, Float, Float, Float) = (0, 0, 0, 0)
            var paint: PagePaint = .solid
            if ver >= 7 {
                radius = (reader.f32(), reader.f32(), reader.f32(), reader.f32())
                let tag = reader.byte()
                switch tag {
                case 1:
                    let angle = reader.f32()
                    let stops = reader.stops()
                    paint = .linear(angle: angle, stops: stops ?? [])
                case 2:
                    let cx = reader.f32(), cy = reader.f32()
                    let hasR = reader.byte()
                    let r: Float? = (hasR == 1) ? reader.f32() : nil
                    let stops = reader.stops()
                    paint = .radial(cx: cx, cy: cy, radius: r, stops: stops ?? [])
                case 3:
                    paint = .image(url: reader.string() ?? "")
                case 4:
                    // B5 conic: angle + fractional center + sweep stops.
                    let angle = reader.f32()
                    let cx = reader.f32(), cy = reader.f32()
                    let stops = reader.stops()
                    paint = .conic(angle: angle, cx: cx, cy: cy, stops: stops ?? [])
                default:
                    break
                }
                guard reader.ok else { return nil }
            }
            f.rects.append(PageRect(x: x, y: y, w: w, h: h, color: color,
                                    radius: radius, paint: paint))
        }

        f.texts.reserveCapacity(Int(textCount))
        for _ in 0..<textCount {
            let x = reader.f32(), y = reader.f32(), size = reader.f32()
            let weight = reader.u32()
            let italic = reader.u32() != 0
            let monospace = reader.u32() != 0
            let color = reader.u32()
            // v10: f32 letter spacing precedes the strings.
            let letterSpacing: Float = (ver >= 10) ? reader.f32() : 0
            // v14: the run's font family (empty = host default).
            let family = (ver >= 14) ? (reader.string() ?? "") : ""
            guard let text = reader.string(), reader.ok else { return nil }
            f.texts.append(PageText(x: x, y: y, size: size, weight: weight, italic: italic,
                                    monospace: monospace, color: color, letterSpacing: letterSpacing,
                                    family: family, text: text))
        }

        f.links.reserveCapacity(Int(linkCount))
        for _ in 0..<linkCount {
            let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
            guard let href = reader.string(), reader.ok else { return nil }
            f.links.append(PageLink(x: x, y: y, w: w, h: h, href: href))
        }

        f.images.reserveCapacity(Int(imageCount))
        for _ in 0..<imageCount {
            let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
            let fit = reader.u32()
            guard let src = reader.string(), let alt = reader.string(), reader.ok else { return nil }
            // v4: u64 payload id appended to each image entry.
            let payload: UInt64 = (ver >= 4) ? reader.u64() : 0
            // v13: animated byte (Q8 GIF frames).
            let animated = (ver >= 13) ? reader.byte() != 0 : false
            f.images.append(PageImage(x: x, y: y, w: w, h: h, fit: fit, src: src, alt: alt,
                                      payload: payload, animated: animated))
        }

        if ver >= 3 {
            guard let focused = reader.string(), let selection = reader.string(), reader.ok else { return nil }
            f.focusedId = focused
            f.selection = selection
        }

        // v5: media section (present flag; then kind/duration/rate/channels/
        // playing/position/url/caf_path — see kitae_core.h).
        if ver >= 5 {
            let present = reader.byte()
            guard reader.ok else { return nil }
            if present == 1 {
                let kind = reader.byte()
                let duration = reader.f32()
                let rate = reader.u32()
                let channels = reader.byte()
                let playing = reader.byte()
                let position = reader.f32()
                guard let url = reader.string(), let caf = reader.string(), reader.ok,
                      kind <= 1
                else { return nil }
                f.media = MediaInfo(
                    kind: kind == 0 ? .audio : .video,
                    duration: duration,
                    rate: rate,
                    channels: channels,
                    playing: playing != 0,
                    position: position,
                    url: url,
                    cafPath: caf)
            }
        }

        // v6: media items (count + per-item id/rect/src/transport).
        if ver >= 6 {
            let count = reader.u32()
            guard reader.ok, count <= 1_000_000 else { return nil }
            f.mediaItems.reserveCapacity(Int(count))
            for _ in 0..<count {
                let id = reader.u64()
                let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
                guard let src = reader.string(), reader.ok else { return nil }
                let kind = reader.byte()
                let playing = reader.byte()
                let position = reader.f32()
                // v20: per-item controls flag (F4) — the engine demuxed this
                // element into the web-media store; the host paints chrome.
                let controls = (ver >= 20) ? reader.byte() != 0 : false
                guard reader.ok, kind <= 1 else { return nil }
                f.mediaItems.append(MediaItem(
                    id: id,
                    x: x, y: y, w: w, h: h,
                    src: src,
                    kind: kind == 0 ? .audio : .video,
                    playing: playing != 0,
                    position: position,
                    controls: controls))
            }
        }

        // v7: box shadows (count + border box, radii, color, blur, spread,
        // offsets, inset flag).
        if ver >= 7 {
            let count = reader.u32()
            guard reader.ok, count <= 1_000_000 else { return nil }
            f.shadows.reserveCapacity(Int(count))
            for _ in 0..<count {
                let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
                let radius = (reader.f32(), reader.f32(), reader.f32(), reader.f32())
                let color = reader.u32()
                let blur = reader.f32(), spread = reader.f32()
                let ox = reader.f32(), oy = reader.f32()
                let inset = reader.byte() != 0
                guard reader.ok else { return nil }
                f.shadows.append(PageShadow(
                    x: x, y: y, w: w, h: h, radius: radius, color: color,
                    blur: blur, spread: spread, offsetX: ox, offsetY: oy, inset: inset))
            }
        }

        // v8: find-in-page (match count, active index with 0xFFFFFFFF =
        // none, then per-match rect lists) and the AX side-table (node
        // count, then id/role/label/rect/parent per node, then focused id).
        if ver >= 8 {
            let matchCount = reader.u32()
            let activeRaw = reader.u32()
            guard reader.ok, matchCount <= 100_000 else { return nil }
            f.findMatches.reserveCapacity(Int(matchCount))
            for _ in 0..<matchCount {
                let rectCount = reader.u32()
                guard reader.ok, rectCount <= 10_000 else { return nil }
                var rects: [(x: Float, y: Float, w: Float, h: Float)] = []
                rects.reserveCapacity(Int(rectCount))
                for _ in 0..<rectCount {
                    let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
                    guard reader.ok else { return nil }
                    rects.append((x, y, w, h))
                }
                f.findMatches.append(FindMatch(rects: rects))
            }
            if activeRaw != 0xFFFF_FFFF, activeRaw < matchCount {
                f.findActive = Int(activeRaw)
            }

            let axCount = reader.u32()
            guard reader.ok, axCount <= 100_000 else { return nil }
            f.axNodes.reserveCapacity(Int(axCount))
            f.axParents.reserveCapacity(Int(axCount))
            var axParentRaw: [UInt32] = []
            axParentRaw.reserveCapacity(Int(axCount))
            for _ in 0..<axCount {
                guard let id = reader.string(), reader.ok else { return nil }
                let role = reader.byte()
                guard let label = reader.string(), reader.ok else { return nil }
                let x = reader.f32(), y = reader.f32(), w = reader.f32(), h = reader.f32()
                let parent = reader.u32()
                guard reader.ok else { return nil }
                f.axNodes.append(AxNode(id: id, role: role, label: label,
                                        x: x, y: y, w: w, h: h, children: []))
                axParentRaw.append(parent)
            }
            // Second pass: link children (parents always precede children —
            // the engine flattens pre-order).
            for (index, parent) in axParentRaw.enumerated() {
                if parent == 0xFFFF_FFFF {
                    f.axParents.append(nil)
                } else if parent < axCount {
                    f.axParents.append(Int(parent))
                    f.axNodes[Int(parent)].children.append(index)
                } else {
                    return nil
                }
            }
            guard let focused = reader.string(), reader.ok else { return nil }
            f.axFocusedId = focused
        }

        if ver >= 9 {
            f.qtsqLocked = reader.byte() != 0
        }

        // v11: signature verdict byte (0 unsigned, 1 valid, 2 invalid,
        // 3 unverified) — mirrors kitae_core's QtsqSigState.
        if ver >= 11 {
            f.qtsqSig = switch reader.byte() {
            case 1: .valid
            case 2: .invalid
            case 3: .unverified
            default: .unsigned
            }
        }

        // v12: sparse item transform tail (Q2) — (kind u8, index u32, 2×3
        // row-major f32 matrix) per transformed item, in rects → texts →
        // images → shadows → links order. Unknown kinds / stale indices are
        // skipped; a truncated tail fails.
        if ver >= 12 {
            let count = reader.u32()
            guard reader.ok, count <= 1_000_000 else { return nil }
            for _ in 0..<count {
                let kind = reader.byte()
                let index = reader.u32()
                let m = PageTransform(m0: reader.f32(), m1: reader.f32(), m2: reader.f32(),
                                      m3: reader.f32(), m4: reader.f32(), m5: reader.f32())
                guard reader.ok else { return nil }
                switch kind {
                case 0 where Int(index) < f.rects.count: f.rects[Int(index)].transform = m
                case 1 where Int(index) < f.texts.count: f.texts[Int(index)].transform = m
                case 2 where Int(index) < f.images.count: f.images[Int(index)].transform = m
                case 3 where Int(index) < f.shadows.count: f.shadows[Int(index)].transform = m
                case 4 where Int(index) < f.links.count: f.links[Int(index)].transform = m
                default: break
                }
            }
        }

        // v15: sticky side-list (Q5) — painted-item ranges per box + the
        // stick metadata. Indices are bounds-checked against the lists.
        if ver >= 15 {
            let count = reader.u32()
            guard reader.ok, count <= 100_000 else { return nil }
            for _ in 0..<count {
                func pair() -> (UInt32, UInt32)? {
                    let s = reader.u32()
                    let c = reader.u32()
                    return reader.ok ? (s, c) : nil
                }
                guard let rect = pair(), let text = pair(), let link = pair(),
                      let image = pair(), let media = pair(), let shadow = pair()
                else { return nil }
                let baseY = reader.f32()
                let height = reader.f32()
                let containerTop = reader.f32()
                let containerBottom = reader.f32()
                let topOffset = reader.f32()
                let bottomOffset = reader.f32()
                let hasTop = reader.byte() != 0
                let hasBottom = reader.byte() != 0
                guard reader.ok else { return nil }
                // Stale/garbage indices clamp to nothing rather than fail —
                // the painter treats an empty range as absent.
                func clamp(_ r: (UInt32, UInt32), _ n: Int) -> (UInt32, UInt32) {
                    let start = min(r.0, UInt32(n))
                    let end = min(r.0 &+ r.1, UInt32(n))
                    return (start, end &- start)
                }
                f.stickies.append(PageSticky(
                    rect: clamp(rect, f.rects.count),
                    text: clamp(text, f.texts.count),
                    link: clamp(link, f.links.count),
                    image: clamp(image, f.images.count),
                    media: clamp(media, f.mediaItems.count),
                    shadow: clamp(shadow, f.shadows.count),
                    baseY: baseY, height: height,
                    containerTop: containerTop, containerBottom: containerBottom,
                    topOffset: topOffset, bottomOffset: bottomOffset,
                    hasTop: hasTop, hasBottom: hasBottom))
            }
        }

        // v16: per-item filter chains (B4) — sparse; only items the styled
        // node actually filtered. The host applies them at paint time
        // (CIFilter); the engine never rasterizes filters itself.
        if ver >= 16 {
            // Wire tags match kitae_core/src/ffi.rs `put_filter_fn`.
            func filterFn() -> PageFilterFn? {
                let tag = reader.byte()
                switch tag {
                case 0: return .blur(reader.f32())
                case 1:
                    let dx = reader.f32(), dy = reader.f32(), blur = reader.f32()
                    return .dropShadow(dx: dx, dy: dy, blur: blur, color: reader.u32())
                case 2: return .brightness(reader.f32())
                case 3: return .contrast(reader.f32())
                case 4: return .grayscale(reader.f32())
                case 5: return .hueRotate(reader.f32())
                case 6: return .invert(reader.f32())
                case 7: return .opacity(reader.f32())
                case 8: return .saturate(reader.f32())
                case 9: return .sepia(reader.f32())
                default: return nil
                }
            }
            let count = reader.u32()
            guard reader.ok, count <= 100_000 else { return nil }
            for _ in 0..<count {
                let kind = reader.byte()
                let index = reader.u32()
                let fncount = reader.u32()
                guard reader.ok else { return nil }
                var fns: [PageFilterFn] = []
                fns.reserveCapacity(Int(min(fncount, 64)))
                for _ in 0..<fncount {
                    guard let fn = filterFn() else { return nil }
                    fns.append(fn)
                }
                guard reader.ok else { return nil }
                let entry = PageFilterEntry(kind: Int(kind), index: Int(index), fns: fns)
                f.filters.append(entry)
            }
        }

        // v17: scroll containers (B2) — engine-side signal only. The host
        // paints the scrollbar and routes wheel events; entries tell it
        // where each pane is and how tall its content is.
        if ver >= 17 {
            let count = reader.u32()
            guard reader.ok, count <= 100_000 else { return nil }
            for _ in 0..<count {
                let node = reader.u32()
                let x = reader.f32()
                let y = reader.f32()
                let w = reader.f32()
                let h = reader.f32()
                let contentH = reader.f32()
                guard reader.ok else { return nil }
                f.scrollContainers.append(PageScrollContainer(
                    node: node, x: x, y: y, w: w, h: h,
                    contentWidth: w, contentHeight: contentH,
                    mode: 1, parent: UInt32.max))
            }
        }

        // v18: per-item clip-path/mask (B7) + blend/group (B8) — sparse.
        // Shapes in page space, indices bounds-checked at paint time.
        // Tags mirror ffi.rs `put_clip`: circle 1, ellipse 2, inset 3,
        // polygon 4; 0 carries a blend/group-only entry (nil clip). Every
        // entry is kept so a nil-clip stamp still delivers its blend id
        // and isolation group.
        if ver >= 18 {
            let count = reader.u32()
            guard reader.ok, count <= 100_000 else { return nil }
            for _ in 0..<count {
                let kind = reader.byte()
                let index = reader.u32()
                let tag = reader.byte()
                var clip: PageClip?
                switch tag {
                case 1:
                    clip = .circle(cx: reader.f32(), cy: reader.f32(), r: reader.f32())
                case 2:
                    clip = .ellipse(cx: reader.f32(), cy: reader.f32(),
                                    rx: reader.f32(), ry: reader.f32())
                case 3:
                    let x = reader.f32(), y = reader.f32()
                    let w = reader.f32(), h = reader.f32()
                    var radii: [(Float, Float)] = []
                    radii.reserveCapacity(4)
                    for _ in 0..<4 { radii.append((reader.f32(), reader.f32())) }
                    clip = .inset(x: x, y: y, w: w, h: h, radii: radii)
                case 4:
                    let evenOdd = reader.byte() != 0
                    let n = reader.u32()
                    guard reader.ok, n <= 4096 else { return nil }
                    var pts: [(Float, Float)] = []
                    pts.reserveCapacity(Int(n))
                    for _ in 0..<n { pts.append((reader.f32(), reader.f32())) }
                    clip = .polygon(evenOdd: evenOdd, points: pts)
                default:
                    break // tag 0: blend/group-only entry, no clip geometry
                }
                let blend = reader.byte()
                let group = reader.u32()
                guard reader.ok else { return nil }
                f.clips.append(PageClipEntry(kind: Int(kind), index: Int(index),
                                             clip: clip, blend: blend, group: group))
            }
        }

        // v19: per-text shadow blur (E3) — sparse (index u32, blur f32)
        // pairs for R8 text-shadow duplicate runs. Stale indices are
        // skipped; every other run keeps blur 0 (crisp).
        if ver >= 19 {
            let count = reader.u32()
            guard reader.ok, count <= 1_000_000 else { return nil }
            for _ in 0..<count {
                let index = reader.u32()
                let blur = reader.f32()
                guard reader.ok else { return nil }
                if Int(index) < f.texts.count {
                    f.texts[Int(index)].shadowBlur = blur
                }
            }
        }

        func decodeAxSemantics(_ reader: inout Reader, _ frame: inout EngineFrame) -> Bool {
            guard reader.u32() == Self.axSemanticsMagic,
                  reader.u32() == 1 else { return false }
            let payloadLength = Int(reader.u32())
            guard reader.ok, payloadLength >= 4, payloadLength <= reader.remaining else {
                return false
            }
            let end = reader.pos + payloadLength
            let count = reader.u32()
            guard reader.ok, count <= 100_000, reader.pos <= end else { return false }
            var seen = Set<Int>()
            for _ in 0..<count {
                let nodeIndex = Int(reader.u32())
                let flags = reader.byte()
                let checkedRaw = reader.byte()
                let headingLevel = reader.byte()
                let liveRaw = reader.byte()
                let valueMin = reader.f32()
                let valueMax = reader.f32()
                let value = reader.string()
                guard reader.ok, nodeIndex < frame.axNodes.count,
                      seen.insert(nodeIndex).inserted, headingLevel <= 6,
                      let checked = AxChecked(rawValue: checkedRaw),
                      let live = AxLivePoliteness(rawValue: liveRaw), value != nil else {
                    return false
                }
                var node = frame.axNodes[nodeIndex]
                node.disabled = flags & 1 != 0
                node.checked = checked
                node.expanded = flags & 2 != 0 ? flags & 4 != 0 : nil
                node.selected = flags & 8 != 0 ? flags & 16 != 0 : nil
                node.headingLevel = headingLevel
                node.livePoliteness = live
                node.value = flags & 32 != 0 ? value : nil
                node.valueMin = flags & 64 != 0 ? valueMin : nil
                node.valueMax = flags & 128 != 0 ? valueMax : nil
                frame.axNodes[nodeIndex] = node
            }
            return reader.ok && reader.pos == end
        }

        if reader.remaining >= 4, reader.peekU32() == Self.axSemanticsMagic {
            guard decodeAxSemantics(&reader, &f) else { return nil }
        } else if reader.remaining >= 4 {
            let count = reader.u32()
            guard reader.ok, Int(count) == f.scrollContainers.count else { return nil }
            for index in 0..<f.scrollContainers.count {
                let contentWidth = reader.f32()
                let mode = reader.byte()
                let parent = reader.u32()
                guard reader.ok else { return nil }
                f.scrollContainers[index].contentWidth = contentWidth
                f.scrollContainers[index].mode = mode
                f.scrollContainers[index].parent = parent
            }
            if reader.remaining >= 4, reader.peekU32() == Self.axSemanticsMagic {
                guard decodeAxSemantics(&reader, &f) else { return nil }
            }
        }

        guard reader.ok else { return nil }
        return f
    }

    /// True when pageBackground carries an opaque color the canvas should
    /// paint instead of the theme default.
    var opaquePageBackground: Bool {
        pageBackground & 0xFF == 0xFF
    }

    var nsPageBackground: NSColor {
        guard opaquePageBackground else { return .clear }
        return Self.color(pageBackground)
    }

    /// 0xRRGGBBAA → NSColor.
    static func color(_ c: UInt32) -> NSColor {
        NSColor(red: CGFloat((c >> 24) & 0xFF) / 255.0,
                green: CGFloat((c >> 16) & 0xFF) / 255.0,
                blue: CGFloat((c >> 8) & 0xFF) / 255.0,
                alpha: CGFloat(c & 0xFF) / 255.0)
    }

    /// Sequential little-endian reader with a hard "all-or-nothing" rule:
    /// any over-read poisons `ok` and the whole decode fails.
    private struct Reader {
        let bytes: [UInt8]
        var pos = 0
        var ok = true

        init(_ bytes: [UInt8]) { self.bytes = bytes }

        var remaining: Int { max(0, bytes.count - pos) }

        func peekU32() -> UInt32? {
            guard pos + 4 <= bytes.count else { return nil }
            return UInt32(bytes[pos])
                | UInt32(bytes[pos + 1]) << 8
                | UInt32(bytes[pos + 2]) << 16
                | UInt32(bytes[pos + 3]) << 24
        }

        mutating func u32() -> UInt32 {
            guard ok, pos + 4 <= bytes.count else { ok = false; return 0 }
            let v = UInt32(bytes[pos])
                | UInt32(bytes[pos + 1]) << 8
                | UInt32(bytes[pos + 2]) << 16
                | UInt32(bytes[pos + 3]) << 24
            pos += 4
            return v
        }

        mutating func u64() -> UInt64 {
            let lo = UInt64(u32())
            let hi = UInt64(u32())
            return lo | hi << 32
        }

        mutating func f32() -> Float {
            Float(bitPattern: u32())
        }

        mutating func byte() -> UInt8 {
            guard ok, pos + 1 <= bytes.count else { ok = false; return 0 }
            defer { pos += 1 }
            return bytes[pos]
        }

        /// Gradient stop list: u32 count + (f32 offset, u32 color) pairs.
        /// Nil when the count is absurd (poisons `ok` via the guard).
        mutating func stops() -> [(offset: Float, color: UInt32)]? {
            let n = u32()
            guard ok, n <= 32 else { ok = false; return nil }
            var out: [(Float, UInt32)] = []
            out.reserveCapacity(Int(n))
            for _ in 0..<n {
                out.append((f32(), u32()))
            }
            return out
        }

        mutating func string() -> String? {
            let len = Int(u32())
            guard ok, len <= bytes.count - pos else { ok = false; return nil }
            defer { pos += len }
            return String(bytes: bytes[pos..<pos + len], encoding: .utf8)
        }
    }
}
