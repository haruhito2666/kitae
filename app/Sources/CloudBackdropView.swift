// CloudBackdropView.swift — GPU-accelerated continuous generative cloud animation for Kitae.
//
// Generates an organic, non-repeating, continuous fluid cloudscape blending
// light peach and light blue gradients using a domain-warped Fractional Brownian
// Motion (fBm) Metal shader. Runs at 60 FPS directly on the GPU with zero lag,
// zero video loops, and minimal battery overhead.

import AppKit
import Metal
import MetalKit

final class CloudBackdropView: NSView, MTKViewDelegate {

    private var mtkView: MTKView?
    private var device: MTLDevice?
    private var commandQueue: MTLCommandQueue?
    private var pipelineState: MTLRenderPipelineState?
    private var vertexBuffer: MTLBuffer?

    private var startTime: CFAbsoluteTime = CFAbsoluteTimeGetCurrent()
    private var displayLinkTimer: Timer?
    /// P-1 (SECURITY_AUDIT.md): 1 Hz key-window heartbeat that pauses the
    /// shader while the window is unfocused.
    private var keyHeartbeat: Timer?
    private let visualEffectView = NSVisualEffectView()
    private var isMetalAvailable = false

    // Fallback CALayer gradient if Metal is unavailable
    private var fallbackLayer: CAGradientLayer?

    private struct Uniforms {
        var resolution: SIMD2<Float>
        var time: Float
        var isDark: Float
    }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true

        visualEffectView.material = .underWindowBackground
        visualEffectView.blendingMode = .behindWindow
        visualEffectView.state = .active
        visualEffectView.autoresizingMask = [.width, .height]
        addSubview(visualEffectView)

        setupMetal()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    // MARK: - Metal Setup

    private func setupMetal() {
        guard let mtlDevice = MTLCreateSystemDefaultDevice(),
              let queue = mtlDevice.makeCommandQueue() else {
            setupFallback()
            return
        }

        self.device = mtlDevice
        self.commandQueue = queue

        let shaderSource = """
        #include <metal_stdlib>
        using namespace metal;

        struct VertexIn {
            float2 position [[attribute(0)]];
        };

        struct VertexOut {
            float4 position [[position]];
            float2 uv;
        };

        struct Uniforms {
            float2 resolution;
            float time;
            float isDark;
        };

        vertex VertexOut cloudVertex(uint vertexID [[vertex_id]]) {
            // Fullscreen triangle covering clip space [-1, 1]
            float2 positions[3] = {
                float2(-1.0, -1.0),
                float2( 3.0, -1.0),
                float2(-1.0,  3.0)
            };
            VertexOut out;
            out.position = float4(positions[vertexID], 0.0, 1.0);
            out.uv = (positions[vertexID] + 1.0) * 0.5;
            return out;
        }

        float hash21(float2 p) {
            p = fract(p * float2(123.34, 456.21));
            p += dot(p, p + 45.32);
            return fract(p.x * p.y);
        }

        float noise(float2 p) {
            float2 i = floor(p);
            float2 f = fract(p);
            float2 u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);

            float a = hash21(i + float2(0.0, 0.0));
            float b = hash21(i + float2(1.0, 0.0));
            float c = hash21(i + float2(0.0, 1.0));
            float d = hash21(i + float2(1.0, 1.0));

            return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
        }

        float fbm(float2 p) {
            float v = 0.0;
            float a = 0.5;
            float2x2 rot = float2x2(0.80, 0.60, -0.60, 0.80);
            for (int i = 0; i < 5; i++) {
                v += a * noise(p);
                p = rot * p * 2.02 + float2(11.3, 17.5);
                a *= 0.5;
            }
            return v;
        }

        fragment float4 cloudFragment(
            VertexOut in [[stage_in]],
            constant Uniforms &u [[buffer(0)]]
        ) {
            float aspect = u.resolution.x / max(1.0, u.resolution.y);
            float2 p = in.uv * float2(aspect, 1.0) * 1.6;

            float t = u.time * 0.038; // Gentle, elegant drift speed

            // Domain warping with incommensurate velocity vectors
            float2 v1 = float2(0.32, 0.16) * t;
            float2 v2 = float2(-0.20, 0.26) * t;

            float2 q;
            q.x = fbm(p + v1);
            q.y = fbm(p + float2(5.2, 1.3) + v2);

            float2 v3 = float2(0.14, -0.18) * t;
            float2 r;
            r.x = fbm(p + 3.0 * q + float2(1.7, 9.2) + v3);
            r.y = fbm(p + 3.0 * q + float2(8.3, 2.8) - v1);

            float f = fbm(p + 3.4 * r + float2(0.0, 0.04 * sin(t * 0.6)));

            // Cloud shaping: soft organic gradient ramp
            // Light Blue base and atmosphere:
            float3 colDeepBlue = float3(0.74, 0.87, 0.98); // #BDE0FA
            float3 colSkyBlue  = float3(0.85, 0.93, 0.99); // #D9EEFC

            // Light Peach tones:
            float3 colLightPeach = float3(1.0, 0.87, 0.79);  // #FFDEC9
            float3 colWarmPeach  = float3(0.99, 0.78, 0.68);  // #FCC7AE
            float3 colPearlCrest = float3(1.0, 0.965, 0.94); // #FFF6F0

            float3 color;
            if (f < 0.35) {
                color = mix(colDeepBlue, colSkyBlue, f / 0.35);
            } else if (f < 0.58) {
                color = mix(colSkyBlue, colLightPeach, (f - 0.35) / 0.23);
            } else if (f < 0.80) {
                color = mix(colLightPeach, colWarmPeach, (f - 0.58) / 0.22);
            } else {
                color = mix(colWarmPeach, colPearlCrest, (f - 0.80) / 0.20);
            }

            // Atmospheric sky gradient blend across window
            float3 skyGrad = mix(float3(0.80, 0.89, 0.98), float3(1.0, 0.88, 0.82), in.uv.y * 0.7 + in.uv.x * 0.3);
            color = mix(skyGrad, color, 0.86);

            if (u.isDark > 0.5) {
                // Dark appearance: soft dusky twilight peach & deep midnight blue
                float3 darkBase = float3(0.09, 0.09, 0.12);
                float3 darkBlue = float3(0.13, 0.16, 0.24);
                float3 darkPeach = float3(0.32, 0.22, 0.21);
                color = mix(darkBase, mix(darkBlue, darkPeach, f), 0.75);
            }

            return float4(color, 0.88);
        }
        """

        do {
            let library = try mtlDevice.makeLibrary(source: shaderSource, options: nil)
            let vertexFunction = library.makeFunction(name: "cloudVertex")
            let fragmentFunction = library.makeFunction(name: "cloudFragment")

            let pipelineDescriptor = MTLRenderPipelineDescriptor()
            pipelineDescriptor.vertexFunction = vertexFunction
            pipelineDescriptor.fragmentFunction = fragmentFunction
            if let attachment = pipelineDescriptor.colorAttachments[0] {
                attachment.pixelFormat = .bgra8Unorm
                attachment.isBlendingEnabled = true
                attachment.rgbBlendOperation = .add
                attachment.alphaBlendOperation = .add
                attachment.sourceRGBBlendFactor = .sourceAlpha
                attachment.destinationRGBBlendFactor = .oneMinusSourceAlpha
                attachment.sourceAlphaBlendFactor = .one
                attachment.destinationAlphaBlendFactor = .oneMinusSourceAlpha
            }

            self.pipelineState = try mtlDevice.makeRenderPipelineState(descriptor: pipelineDescriptor)

            let view = MTKView(frame: bounds, device: mtlDevice)
            view.delegate = self
            view.clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 0)
            view.colorPixelFormat = .bgra8Unorm
            // P-1 (SECURITY_AUDIT.md): the backdrop is decorative — 30 fps
            // is visually indistinguishable for drifting clouds and halves
            // the GPU work versus the old flat 60.
            view.preferredFramesPerSecond = 30
            view.isPaused = true
            view.enableSetNeedsDisplay = false
            view.layer?.isOpaque = false
            if let metalLayer = view.layer as? CAMetalLayer {
                metalLayer.isOpaque = false
            }
            view.autoresizingMask = [.width, .height]
            addSubview(view)
            self.mtkView = view
            self.isMetalAvailable = true
        } catch {
            setupFallback()
        }
    }

    private func setupFallback() {
        let grad = CAGradientLayer()
        grad.frame = bounds
        grad.colors = [
            NSColor(red: 0.85, green: 0.93, blue: 0.99, alpha: 0.88).cgColor,
            NSColor(red: 1.0, green: 0.87, blue: 0.79, alpha: 0.88).cgColor,
            NSColor(red: 0.74, green: 0.87, blue: 0.98, alpha: 0.88).cgColor
        ]
        grad.startPoint = CGPoint(x: 0, y: 0)
        grad.endPoint = CGPoint(x: 1, y: 1)
        layer?.addSublayer(grad)
        fallbackLayer = grad
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        // P-1 (SECURITY_AUDIT.md): run only while we are actually in a key
        // window — visible and focused. A background/unfocused app must not
        // burn the GPU on a decorative shader.
        updateRendering(enabled: window != nil && windowIsKey)
        if window == nil {
            keyHeartbeat?.invalidate()
            keyHeartbeat = nil
        } else if keyHeartbeat == nil {
            // 1 Hz heartbeat: NSView has no key-window callback, so re-check
            // focus cheaply. Negligible power vs. a GPU shader at any rate.
            let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
                self?.updateRendering(enabled: self?.windowIsKey ?? false)
            }
            RunLoop.main.add(timer, forMode: .common)
            keyHeartbeat = timer
        }
    }

    /// The window thinks it is the focused key window. When we cannot tell
    /// (embedded/offscreen contexts), be conservative and animate — the
    /// frame cap already limits the cost.
    private var windowIsKey: Bool {
        window?.isKeyWindow ?? true
    }

    private func updateRendering(enabled: Bool) {
        mtkView?.isPaused = !enabled
        if enabled {
            needsDisplay = true
        }
    }

    override func layout() {
        super.layout()
        visualEffectView.frame = bounds
        mtkView?.frame = bounds
        fallbackLayer?.frame = bounds
    }

    // MARK: - MTKViewDelegate

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        guard let drawable = view.currentDrawable,
              let renderPassDescriptor = view.currentRenderPassDescriptor,
              let pipelineState = self.pipelineState,
              let commandQueue = self.commandQueue else {
            return
        }

        let isDark: Float = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? 1.0 : 0.0
        let elapsed = Float(CFAbsoluteTimeGetCurrent() - startTime)
        let size = view.drawableSize

        var uniforms = Uniforms(
            resolution: SIMD2<Float>(Float(size.width), Float(size.height)),
            time: elapsed,
            isDark: isDark
        )

        guard let commandBuffer = commandQueue.makeCommandBuffer(),
              let renderEncoder = commandBuffer.makeRenderCommandEncoder(descriptor: renderPassDescriptor) else {
            return
        }

        renderEncoder.setRenderPipelineState(pipelineState)
        renderEncoder.setFragmentBytes(&uniforms, length: MemoryLayout<Uniforms>.size, index: 0)
        renderEncoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        renderEncoder.endEncoding()

        commandBuffer.present(drawable)
        commandBuffer.commit()
    }
}
