// MetalCanvasView.swift — GPU-painted page layers behind the text overlay.
//
// Layer-hosting CAMetalLayer view (Phase L5): each engine frame is rendered
// offscreen by kitae_render_gpu (Metal when available, CPU reference as
// fallback) and presented as a single blit. Text stays Core Text: the
// PageCanvasView in front runs in `textOnly` mode and paints runs +
// scrollbar over this bitmap. When presentation fails the view hides and
// the canvas falls back to full CPU painting — the permanent proof of the
// fallback story (doubles as the L5 Settings-toggle demo).

import AppKit
import Metal

enum RenderSettings {
    static var gpuOn: Bool {
        get { SettingsStore.shared.gpuEnabled }
        set { SettingsStore.shared.gpuEnabled = newValue }
    }
}


final class MetalCanvasView: NSView {
    /// True when this machine can present through Metal at all.
    static var isAvailable: Bool { MTLCreateSystemDefaultDevice() != nil }

    private var device: MTLDevice?
    private var queue: MTLCommandQueue?
    private var metalLayer: CAMetalLayer?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        // (No `wantsUpdateLayer` assignment: it is get-only in this SDK and
        // defaults to false, which is what this manually-driven layer wants.)
        let layer = CAMetalLayer()
        layer.pixelFormat = .rgba8Unorm
        layer.framebufferOnly = false
        layer.contentsScale = window?.backingScaleFactor ?? 2.0
        self.layer = layer
        metalLayer = layer
        device = MTLCreateSystemDefaultDevice()
        queue = device?.makeCommandQueue()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func clearCache() {
        metalLayer?.contents = nil
        needsDisplay = true
    }

    override func layout() {
        super.layout()
        metalLayer?.drawableSize = CGSize(
            width: max(1, bounds.width * (window?.backingScaleFactor ?? 2.0)),
            height: max(1, bounds.height * (window?.backingScaleFactor ?? 2.0)))
    }

    /// Present one RGBA8 tile (`w*h*4` bytes, straight alpha, top row
    /// first). Returns false when Metal is unavailable or the drawable is
    /// busy — the caller falls back to CPU painting.
    @discardableResult
    func present(pixels: [UInt8], width w: Int, height h: Int) -> Bool {
        guard w > 0, h > 0, pixels.count >= w * h * 4,
              let device, let queue,
              let layer = metalLayer,
              let drawable = layer.nextDrawable()
        else { return false }
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .rgba8Unorm, width: w, height: h, mipmapped: false)
        desc.usage = [.shaderRead, .renderTarget]
        guard let src = device.makeTexture(descriptor: desc) else { return false }
        src.replace(region: MTLRegionMake2D(0, 0, w, h), mipmapLevel: 0,
                    withBytes: pixels, bytesPerRow: w * 4)
        guard let cmd = queue.makeCommandBuffer(),
              let blit = cmd.makeBlitCommandEncoder()
        else { return false }
        blit.copy(from: src,
                  sourceSlice: 0, sourceLevel: 0,
                  sourceOrigin: MTLOrigin(x: 0, y: 0, z: 0),
                  sourceSize: MTLSize(width: w, height: h, depth: 1),
                  to: drawable.texture,
                  destinationSlice: 0, destinationLevel: 0,
                  destinationOrigin: MTLOrigin(x: 0, y: 0, z: 0))
        blit.endEncoding()
        cmd.present(drawable)
        cmd.commit()
        return true
    }
}
