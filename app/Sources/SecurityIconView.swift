import AppKit
import QuartzCore

// SecurityIconView — security icon in the search pill, Lottie-driven.
//   - hidden: nothing shown (empty tab, about: pages)
//   - loading: spinning arc (native CAShapeLayer, as before)
//   - secure (https): Pull-to-Refresh.lottie, played once and held
//   - insecure (http): Alert.lottie, played once and held
// Play-once-then-hold: the animation runs once on each state entry and the
// final frame persists until the state changes again.
//
// P-3 (SECURITY_AUDIT.md): native Core Animation rendering, no WKWebView,
// no JS engine, no WebContent process.
//
// Memory note: there is no WKScriptMessageHandler anymore, so the old retain
// cycle (view → webView → userContentController → view) is gone by construction.
final class LottieIconView: NSView {

    enum State: String, Equatable {
        case hidden
        case loading
        case secure
        case insecure
    }

    private let shapeLayer = CAShapeLayer()
    private var secureIcon: LottieView?
    private var insecureIcon: LottieView?
    private var currentState: State = .hidden

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        setupLayer()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }

    // Transparent to clicks: let mouse events pass through to searchContainer
    override func hitTest(_ point: NSPoint) -> NSView? {
        return nil
    }

    private func setupLayer() {
        shapeLayer.frame = bounds
        shapeLayer.fillColor = nil
        shapeLayer.lineWidth = 2
        shapeLayer.lineCap = .round
        shapeLayer.lineJoin = .round
        layer?.addSublayer(shapeLayer)
        if let doc = LottieDocument.named("Pull-to-Refresh") {
            let view = LottieView(document: doc)
            addSubview(view)
            secureIcon = view
        }
        if let doc = LottieDocument.named("Alert") {
            let view = LottieView(document: doc)
            addSubview(view)
            insecureIcon = view
        }
        isHidden = true
        shapeLayer.isHidden = true
        secureIcon?.isHidden = true
        insecureIcon?.isHidden = true
    }

    override func layout() {
        super.layout()
        shapeLayer.frame = bounds
        let iconFrame = bounds.insetBy(dx: 2, dy: 2)
        secureIcon?.frame = iconFrame
        insecureIcon?.frame = iconFrame
    }

    func setState(_ state: State) {
        // Same-state calls are ignored — this is what makes the animation
        // play exactly once per entry and persist until the state changes.
        guard state != currentState else { return }
        currentState = state
        isHidden = (state == .hidden)
        updateShape(for: state)
    }

    private func updateShape(for state: State) {
        shapeLayer.isHidden = (state != .loading)
        secureIcon?.isHidden = (state != .secure)
        insecureIcon?.isHidden = (state != .insecure)

        switch state {
        case .hidden:
            stopSpin()
        case .loading:
            shapeLayer.path = Self.spinnerPath(bounds: bounds)
            shapeLayer.strokeColor = Theme.cgColor(of: Theme.chromeText, for: effectiveAppearance)
            startSpin()
        case .secure:
            stopSpin()
            secureIcon?.playOnce()
        case .insecure:
            stopSpin()
            insecureIcon?.playOnce()
        }
    }

    private static func spinnerPath(bounds: CGRect) -> CGPath {
        let rect = bounds.insetBy(dx: 2, dy: 2)
        let path = CGMutablePath()
        let center = CGPoint(x: rect.midX, y: rect.midY)
        let radius = min(rect.width, rect.height) / 2
        // 3/4 circle — the gap spins with the rotation, reading as a loader.
        path.addArc(center: center, radius: radius, startAngle: 0, endAngle: .pi * 1.5, clockwise: false)
        return path
    }

    private func startSpin() {
        guard Theme.motionEnabled else { return }
        let anim = CABasicAnimation(keyPath: "transform.rotation.z")
        anim.fromValue = 0
        anim.toValue = CGFloat.pi * 2
        anim.duration = 1.0
        anim.repeatCount = .infinity
        anim.isRemovedOnCompletion = false
        shapeLayer.add(anim, forKey: "spin")
    }

    private func stopSpin() {
        shapeLayer.removeAnimation(forKey: "spin")
    }
}
