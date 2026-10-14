import AppKit
import Foundation
import XCTest

private func appendU32(_ value: UInt32, to bytes: inout [UInt8]) {
    bytes.append(UInt8(value & 0xFF))
    bytes.append(UInt8((value >> 8) & 0xFF))
    bytes.append(UInt8((value >> 16) & 0xFF))
    bytes.append(UInt8((value >> 24) & 0xFF))
}

private func appendF32(_ value: Float, to bytes: inout [UInt8]) {
    appendU32(value.bitPattern, to: &bytes)
}

private func appendString(_ value: String, to bytes: inout [UInt8]) {
    let utf8 = Array(value.utf8)
    appendU32(UInt32(utf8.count), to: &bytes)
    bytes.append(contentsOf: utf8)
}

private func nativeMeasure(
    _: UnsafeMutableRawPointer?,
    text: UnsafePointer<CChar>?,
    size: Float,
    weight: UInt32,
    italic: Int32,
    monospace: Int32,
    family: UnsafePointer<CChar>?
) -> Float {
    let value = String(cString: text!)
    let width = value.utf8.count
    let factor: Float = monospace == 0 ? 0.5 : 0.6
    return Float(width) * size * factor
}

private func nativeMetrics(
    _: UnsafeMutableRawPointer?,
    size: Float,
    monospace: Int32,
    ascent: UnsafeMutablePointer<Float>?,
    descent: UnsafeMutablePointer<Float>?,
    family: UnsafePointer<CChar>?
) {
    ascent?.pointee = size * 0.8
    descent?.pointee = size * 0.2
}

private func nativeXHeight(
    _: UnsafeMutableRawPointer?,
    size: Float,
    weight: UInt32,
    italic: Int32,
    monospace: Int32,
    family: UnsafePointer<CChar>?
) -> Float {
    size * 0.5
}

final class NativeContractTests: XCTestCase {
    func testRegistrableDomainUsesThePublicSuffixBoundary() {
        var output = [CChar](repeating: 0, count: 128)
        let length = "https://www.example.co.uk/path".withCString {
            kitae_registrable_domain($0, &output, output.count)
        }
        XCTAssertEqual(String(cString: output), "example.co.uk")
        XCTAssertEqual(length, 14)
    }

    func testEnginePublishesAV20Frame() {
        var vtable = KitaeMeasurerVtable(
            ctx: nil,
            measure: nativeMeasure,
            metrics: nativeMetrics,
            x_height: nativeXHeight
        )
        guard let engine = kitae_engine_new(800, 600, &vtable) else {
            return XCTFail("engine creation failed")
        }
        defer { kitae_engine_destroy(engine) }
        var bytes = [UInt8](repeating: 0, count: 1 << 20)
        var length = 0
        for _ in 0..<100 {
            length = kitae_engine_frame(engine, &bytes, bytes.count)
            if length > 0 { break }
            usleep(10_000)
        }
        XCTAssertGreaterThan(length, 0)
        let data = Array(bytes.prefix(length))
        XCTAssertEqual(data.prefix(4).map { UInt32($0) }, [20, 0, 0, 0])
        XCTAssertNotNil(EngineFrame.decode(data))
    }

    func testEngineFrameDecodesLinkTransformWithLegacyFallback() {
        func makeFrame(transform: [Float]?) -> [UInt8] {
            var bytes = [UInt8]()
            appendU32(20, to: &bytes)
            appendU32(0, to: &bytes)
            appendF32(0, to: &bytes)
            appendF32(200, to: &bytes)
            appendU32(0xFFFFFFFF, to: &bytes)
            appendString("https://example.test/", to: &bytes)
            appendString("Rotated", to: &bytes)
            for count in [UInt32(0), 0, 1, 0] { appendU32(count, to: &bytes) }
            for value in [Float(10), 20, 100, 20] { appendF32(value, to: &bytes) }
            appendString("mailto:rotated@example.test", to: &bytes)
            appendString("", to: &bytes)
            appendString("", to: &bytes)
            bytes.append(0)
            appendU32(0, to: &bytes)
            appendU32(0, to: &bytes)
            appendU32(0, to: &bytes)
            appendU32(UInt32.max, to: &bytes)
            appendU32(0, to: &bytes)
            appendString("", to: &bytes)
            bytes.append(0)
            bytes.append(0)
            if let transform {
                appendU32(1, to: &bytes)
                bytes.append(4)
                appendU32(0, to: &bytes)
                for value in transform { appendF32(value, to: &bytes) }
            } else {
                appendU32(0, to: &bytes)
            }
            for _ in 0..<5 { appendU32(0, to: &bytes) }
            return bytes
        }

        let frame = EngineFrame.decode(makeFrame(transform: [0, -1, 10, 1, 0, 20]))
        XCTAssertNotNil(frame)
        XCTAssertEqual(frame?.links.first?.transform?.m1, -1)
        let rotated = frame?.links.first
        XCTAssertTrue(rotated?.contains(x: -20, y: 80) == true)
        XCTAssertFalse(rotated?.contains(x: 60, y: 30) == true)
        XCTAssertEqual(rotated?.pageBounds, NSRect(x: -30, y: 30, width: 20, height: 100))

        let legacy = EngineFrame.decode(makeFrame(transform: nil))
        XCTAssertNotNil(legacy)
        XCTAssertNil(legacy?.links.first?.transform)
        XCTAssertTrue(legacy?.links.first?.contains(x: 60, y: 30) == true)
        XCTAssertFalse(legacy?.links.first?.contains(x: -20, y: 80) == true)
    }

    func testEngineFrameDecodesOptionalAxSemanticsTail() {
        var bytes = [UInt8]()
        appendU32(20, to: &bytes)
        appendU32(0, to: &bytes)
        appendF32(0, to: &bytes)
        appendF32(100, to: &bytes)
        appendU32(0xFFFFFFFF, to: &bytes)
        appendString("https://example.test/", to: &bytes)
        appendString("Test", to: &bytes)
        for _ in 0..<4 { appendU32(0, to: &bytes) }
        appendString("", to: &bytes)
        appendString("", to: &bytes)
        bytes.append(0)
        appendU32(0, to: &bytes)
        appendU32(0, to: &bytes)
        appendU32(0, to: &bytes)
        appendU32(0xFFFFFFFF, to: &bytes)
        appendU32(1, to: &bytes)
        appendString("heading", to: &bytes)
        bytes.append(1)
        appendString("Heading", to: &bytes)
        for value in [Float(1), 2, 3, 4] { appendF32(value, to: &bytes) }
        appendU32(UInt32.max, to: &bytes)
        appendString("", to: &bytes)
        bytes.append(0)
        bytes.append(0)
        for _ in 0..<6 { appendU32(0, to: &bytes) }
        let legacy = bytes

        appendU32(0, to: &bytes)
        var payload = [UInt8]()
        appendU32(1, to: &payload)
        appendU32(0, to: &payload)
        payload.append(1 | 2 | 4 | 8 | 16 | 32 | 64 | 128)
        payload.append(2)
        payload.append(2)
        payload.append(1)
        appendF32(0, to: &payload)
        appendF32(10, to: &payload)
        appendString("5", to: &payload)
        appendU32(0x31535841, to: &bytes)
        appendU32(1, to: &bytes)
        appendU32(UInt32(payload.count), to: &bytes)
        bytes.append(contentsOf: payload)

        let frame = EngineFrame.decode(bytes)
        XCTAssertNotNil(frame)
        let node = frame?.axNodes.first
        XCTAssertEqual(node?.value, "5")
        XCTAssertEqual(node?.valueMin, 0)
        XCTAssertEqual(node?.valueMax, 10)
        XCTAssertEqual(node?.disabled, true)
        XCTAssertEqual(node?.checked, .checked)
        XCTAssertEqual(node?.expanded, true)
        XCTAssertEqual(node?.selected, true)
        XCTAssertEqual(node?.headingLevel, 2)
        XCTAssertEqual(node?.livePoliteness, .polite)

        let legacyFrame = EngineFrame.decode(legacy)
        XCTAssertNotNil(legacyFrame)
        XCTAssertNil(legacyFrame?.axNodes.first?.value)
        XCTAssertEqual(legacyFrame?.axNodes.first?.headingLevel, 0)

        var truncated = bytes
        truncated.removeLast()
        XCTAssertNil(EngineFrame.decode(truncated))
    }

    /// H3 print media over the C ABI: `KITAE_CMD_SET_MEDIA` (52) makes the
    /// engine re-lay the page out for paper — `@media print` applies,
    /// `@media screen` does not — and flags the frame it publishes (v20
    /// `flags` bit2, `EngineFrame.isPrintMedia`). Sending 0 again puts the
    /// screen frame back. The print path in PagePrinter is built on exactly
    /// this round trip.
    func testSetMediaCommandSwitchesTheFrameToPrintMediaAndBack() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("kitae-print-contract-\(ProcessInfo.processInfo.processIdentifier)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let page = dir.appendingPathComponent("page.html")
        try """
        <html><head><style>
        @media screen { p { color: #020202 } }
        @media print { p { color: #010101 } }
        </style></head><body><p>onpaper</p></body></html>
        """.write(to: page, atomically: true, encoding: .utf8)

        var vtable = KitaeMeasurerVtable(
            ctx: nil,
            measure: nativeMeasure,
            metrics: nativeMetrics,
            x_height: nativeXHeight
        )
        guard let engine = kitae_engine_new(400, 600, &vtable) else {
            return XCTFail("engine creation failed")
        }
        defer { kitae_engine_destroy(engine) }

        func takeFrame(matching predicate: (EngineFrame) -> Bool) -> EngineFrame? {
            for _ in 0..<400 {
                var bytes = [UInt8](repeating: 0, count: 1 << 22)
                let length = kitae_engine_frame(engine, &bytes, bytes.count)
                if length > 0, let frame = EngineFrame.decode(bytes) {
                    if predicate(frame) { return frame }
                } else if length == 0 {
                    usleep(25_000)
                }
            }
            return nil
        }
        func runColor(_ frame: EngineFrame) -> UInt32? {
            frame.texts.first { $0.text.contains("onpaper") }?.color
        }

        page.absoluteString.withCString {
            kitae_engine_command(engine, Int32(KITAE_CMD_NAVIGATE), 0, 0, $0)
        }
        guard let screen = takeFrame(matching: { $0.url.hasSuffix("page.html") }) else {
            return XCTFail("no frame for the fixture page")
        }
        XCTAssertFalse(screen.isPrintMedia)
        XCTAssertEqual(runColor(screen), 0x0202_02FF, "@media screen styles the screen frame")

        kitae_engine_command(engine, Int32(KITAE_CMD_SET_MEDIA), 1, 0, nil)
        guard let printed = takeFrame(matching: { $0.isPrintMedia }) else {
            return XCTFail("no print-media frame after KITAE_CMD_SET_MEDIA")
        }
        XCTAssertEqual(runColor(printed), 0x0101_01FF, "@media print styles the print frame")

        kitae_engine_command(engine, Int32(KITAE_CMD_SET_MEDIA), 0, 0, nil)
        guard let restored = takeFrame(matching: { !$0.isPrintMedia }) else {
            return XCTFail("no screen frame after switching back")
        }
        XCTAssertEqual(runColor(restored), 0x0202_02FF, "screen styles come back")
    }

    /// The exact command sequence `PagePrinter.exportPrintMedia` runs: resize
    /// to the paper's printable width, switch to print media, take the
    /// flagged frame, then put screen media and the screen viewport back. The
    /// full-bleed block proves the print frame really was laid out for paper
    /// (540pt) and the restored one for the window (400pt).
    func testPrintChoreographyLaysOutForPaperThenRestoresTheScreen() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("kitae-print-choreo-\(ProcessInfo.processInfo.processIdentifier)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let page = dir.appendingPathComponent("page.html")
        try """
        <html><head><style>
        html, body { margin: 0; padding: 0 }
        #bleed { width: 100%; height: 20px; background: #101010 }
        </style></head><body><div id="bleed"></div></body></html>
        """.write(to: page, atomically: true, encoding: .utf8)

        var vtable = KitaeMeasurerVtable(
            ctx: nil,
            measure: nativeMeasure,
            metrics: nativeMetrics,
            x_height: nativeXHeight
        )
        guard let engine = kitae_engine_new(400, 600, &vtable) else {
            return XCTFail("engine creation failed")
        }
        defer { kitae_engine_destroy(engine) }

        func takeFrame(matching predicate: (EngineFrame) -> Bool) -> EngineFrame? {
            for _ in 0..<400 {
                var bytes = [UInt8](repeating: 0, count: 1 << 22)
                let length = kitae_engine_frame(engine, &bytes, bytes.count)
                if length > 0, let frame = EngineFrame.decode(bytes) {
                    if predicate(frame) { return frame }
                } else if length == 0 {
                    usleep(25_000)
                }
            }
            return nil
        }
        func bleedWidth(_ frame: EngineFrame) -> Float? {
            frame.rects.first { $0.h >= 19 && $0.h <= 21 && abs($0.y) < 1 }?.w
        }

        page.absoluteString.withCString {
            kitae_engine_command(engine, Int32(KITAE_CMD_NAVIGATE), 0, 0, $0)
        }
        guard let screen = takeFrame(matching: { $0.url.hasSuffix("page.html") }),
              let screenWidth = bleedWidth(screen)
        else { return XCTFail("no frame for the fixture page") }
        XCTAssertEqual(screenWidth, 400, accuracy: 1, "the window width on screen")

        // Paper: 8.5in sheet − 2 × 0.5in margins = 540pt of content.
        kitae_engine_command(engine, Int32(KITAE_CMD_RESIZE), 540, 600, nil)
        _ = takeFrame(matching: { _ in true })
        kitae_engine_command(engine, Int32(KITAE_CMD_SET_MEDIA), 1, 0, nil)
        guard let printed = takeFrame(matching: { $0.isPrintMedia }),
              let paperWidth = bleedWidth(printed)
        else { return XCTFail("no print-media frame for the paper viewport") }
        XCTAssertEqual(paperWidth, 540, accuracy: 1, "print media lays out at the paper width")

        kitae_engine_command(engine, Int32(KITAE_CMD_SET_MEDIA), 0, 0, nil)
        kitae_engine_command(engine, Int32(KITAE_CMD_RESIZE), 400, 600, nil)
        guard let restored = takeFrame(matching: { !$0.isPrintMedia }),
              let restoredWidth = bleedWidth(restored)
        else { return XCTFail("no screen frame after the run") }
        XCTAssertEqual(restoredWidth, 400, accuracy: 1, "the window is back")
    }

    func testDeepLinksAllowOnlySupportedNavigationTargets() {
        XCTAssertEqual(DeepLinkParser.parse("kitae://history"), .history)
        XCTAssertEqual(
            DeepLinkParser.parse("kitae://open?url=https%3A%2F%2Fexample.com%2F&newTab=true"),
            .open(url: "https://example.com/", newTab: true)
        )
        XCTAssertNil(DeepLinkParser.parse("kitae://open?url=file%3A%2F%2F%2Fetc%2Fpasswd"))
        XCTAssertNil(DeepLinkParser.parse("kitae://open?url=javascript%3Aalert(1)"))
    }

    func testInfoPlistRegistersTheDeepLinkScheme() throws {
        let infoURL = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("Info.plist")
        let plist = try PropertyListSerialization.propertyList(
            from: Data(contentsOf: infoURL), options: [], format: nil
        ) as? [String: Any]
        let types = plist?["CFBundleURLTypes"] as? [[String: Any]]
        let schemes = types?.first?["CFBundleURLSchemes"] as? [String]
        XCTAssertEqual(schemes, ["kitae"])
    }

    func testGpuHandleCanBeInitializedAndReleased() {
        guard let handle = kru_gpu_init() else {
            return XCTFail("GPU handle creation failed")
        }
        kru_gpu_shutdown(handle)
    }
}

extension NativeContractTests {
    static let allTests = [
        ("testRegistrableDomainUsesThePublicSuffixBoundary", testRegistrableDomainUsesThePublicSuffixBoundary),
        ("testEnginePublishesAV20Frame", testEnginePublishesAV20Frame),
        ("testEngineFrameDecodesLinkTransformWithLegacyFallback", testEngineFrameDecodesLinkTransformWithLegacyFallback),
        ("testEngineFrameDecodesOptionalAxSemanticsTail", testEngineFrameDecodesOptionalAxSemanticsTail),
        ("testSetMediaCommandSwitchesTheFrameToPrintMediaAndBack", testSetMediaCommandSwitchesTheFrameToPrintMediaAndBack),
        ("testPrintChoreographyLaysOutForPaperThenRestoresTheScreen", testPrintChoreographyLaysOutForPaperThenRestoresTheScreen),
        ("testDeepLinksAllowOnlySupportedNavigationTargets", testDeepLinksAllowOnlySupportedNavigationTargets),
        ("testInfoPlistRegistersTheDeepLinkScheme", testInfoPlistRegistersTheDeepLinkScheme),
        ("testGpuHandleCanBeInitializedAndReleased", testGpuHandleCanBeInitializedAndReleased),
    ]
}
