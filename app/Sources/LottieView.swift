// LottieView.swift — native .lottie (dotLottie) player for chrome UI icons.
//
// P-3 (SECURITY_AUDIT.md): no WKWebView, no JS engine, no WebContent process.
// A .lottie file is a zip containing manifest.json + animations/<id>.json.
// This player reads the zip directly (Compression framework, raw DEFLATE),
// evaluates the Lottie JSON subset used by our bundled icons and rebuilds
// CAShapeLayers per frame. Playback is play-once-then-hold: an animation runs
// from its `ip` frame to `op - 1` and freezes there until the next playOnce().
//
// Renderer subset (validated against lottie-web semantics for our assets):
// layers ty 0/4 + parenting, transforms (o/r/p/s/a, split-dim position,
// spatial to/ti tangents), hold keyframes, bezier easings, shapes gr/sh/rc/sr,
// fills/strokes, trim paths (tm), merge (mm), rounded corners (rd, static).

import AppKit
import QuartzCore
import Compression

// MARK: - JSON helpers

private func lDict(_ v: Any?) -> [String: Any]? { v as? [String: Any] }

private func lArr(_ v: Any?) -> [Any]? { v as? [Any] }

private func lNum(_ v: Any?, _ d: Double = 0) -> Double {
    (v as? NSNumber)?.doubleValue ?? d
}

private func lInt(_ v: Any?, _ d: Int = 0) -> Int {
    (v as? NSNumber)?.intValue ?? d
}

/// Array of numbers; JSON nulls inside become 0.
private func lNums(_ v: Any?) -> [Double]? {
    guard let arr = v as? [Any] else { return nil }
    return arr.map { lNum($0) }
}

// MARK: - Affine math
// (a,b,c,d,e,f): x' = a x + c y + e; y' = b x + d y + f.
// mul(A, B) applies B first, then A — matches the validated reference port.

struct LtMat {
    var a: Double, b: Double, c: Double, d: Double, e: Double, f: Double

    static let identity = LtMat(a: 1, b: 0, c: 0, d: 1, e: 0, f: 0)

    func mul(_ m: LtMat) -> LtMat {
        LtMat(a: a * m.a + c * m.b,
              b: b * m.a + d * m.b,
              c: a * m.c + c * m.d,
              d: b * m.c + d * m.d,
              e: a * m.e + c * m.f + e,
              f: b * m.e + d * m.f + f)
    }

    func apply(_ p: CGPoint) -> CGPoint {
        CGPoint(x: a * p.x + c * p.y + e, y: b * p.x + d * p.y + f)
    }

    static func translate(_ x: Double, _ y: Double) -> LtMat {
        LtMat(a: 1, b: 0, c: 0, d: 1, e: x, f: y)
    }

    static func scale(_ s: Double) -> LtMat {
        LtMat(a: s, b: 0, c: 0, d: s, e: 0, f: 0)
    }
}

/// translate(p) · rotate · scale · translate(-anchor) — Lottie layer transform.
private func ltTrs(_ p: CGPoint, _ rDeg: Double, _ s: [Double], _ a: [Double]) -> LtMat {
    let r = rDeg * .pi / 180
    let cs = cos(r), sn = sin(r)
    let rot = LtMat(a: cs, b: sn, c: -sn, d: cs, e: 0, f: 0)
    let sx = (s.first ?? 0) / 100
    let sy = (s.count > 1 ? s[1] : 0) / 100
    let scl = LtMat(a: sx, b: 0, c: 0, d: sy, e: 0, f: 0)
    let ax = a.first ?? 0
    let ay = a.count > 1 ? a[1] : 0
    return LtMat.translate(p.x, p.y).mul(rot).mul(scl).mul(LtMat.translate(-ax, -ay))
}

private func ltCubic(_ p0: CGPoint, _ p1: CGPoint, _ p2: CGPoint, _ p3: CGPoint, _ u: Double) -> CGPoint {
    let mu = 1 - u
    let m2 = mu * mu, m3 = m2 * mu
    let u2 = u * u, u3 = u2 * u
    return CGPoint(x: m3 * p0.x + 3 * m2 * u * p1.x + 3 * mu * u2 * p2.x + u3 * p3.x,
                   y: m3 * p0.y + 3 * m2 * u * p1.y + 3 * mu * u2 * p2.y + u3 * p3.y)
}

// MARK: - Property evaluation

struct LottieProp {
    struct Kf {
        var t: Double
        var s: [Double]?
        var e: [Double]?
        var h: Bool
        var o: [String: Any]?
        var i: [String: Any]?
        var to: [Double]?
        var ti: [Double]?
    }

    var keyframes: [Kf]?
    var stat: [Double] = [0, 0]

    init(_ d: [String: Any]?) {
        guard let d = d, let k = d["k"] else { return }
        if let arr = lArr(k), arr.first is [String: Any] {
            var kfs: [Kf] = []
            for item in arr {
                guard let kf = item as? [String: Any] else { continue }
                kfs.append(Kf(t: lNum(kf["t"]),
                              s: lNums(kf["s"]),
                              e: lNums(kf["e"]),
                              h: lNum(kf["h"]) == 1,
                              o: lDict(kf["o"]),
                              i: lDict(kf["i"]),
                              to: lNums(kf["to"]),
                              ti: lNums(kf["ti"])))
            }
            if !kfs.isEmpty {
                keyframes = kfs
                return
            }
        } else if let arr = lArr(k) {
            let vals = arr.map { lNum($0) }
            stat = vals.isEmpty ? [0, 0] : vals
        } else if let num = k as? NSNumber {
            stat = [num.doubleValue]
        }
    }

    func at(_ t: Double) -> [Double] {
        guard let kfs = keyframes, !kfs.isEmpty else { return stat }
        if t <= kfs[0].t { return kfs[0].s ?? [0] }
        let last = kfs[kfs.count - 1]
        if t >= last.t {
            if let s = last.s, !s.isEmpty { return s }
            // Legacy s/e keyframes: the final keyframe often has no "s".
            if kfs.count > 1, let e = kfs[kfs.count - 2].e, !e.isEmpty { return e }
            return [0]
        }
        var j = 0
        for idx in 0..<(kfs.count - 1) {
            if kfs[idx].t <= t && t < kfs[idx + 1].t { j = idx; break }
        }
        let a = kfs[j], b = kfs[j + 1]
        let s = a.s ?? [0]
        if a.h { return s }
        let e = a.e ?? b.s ?? [0]
        let span = b.t - a.t
        let u = span != 0 ? (t - a.t) / span : 1.0
        let g = ltEase(a.o, a.i, u)
        var out: [Double] = []
        for (sv, ev) in zip(s, e) { out.append(sv + (ev - sv) * g) }
        return out
    }

    /// Returns the (from, to) keyframes when t is strictly inside an animated
    /// segment whose from-keyframe has spatial tangents (to/ti).
    func segAt(_ t: Double) -> (Kf, Kf)? {
        guard let kfs = keyframes, !kfs.isEmpty else { return nil }
        guard t > kfs[0].t, t < kfs[kfs.count - 1].t else { return nil }
        for idx in 0..<(kfs.count - 1) {
            if kfs[idx].t <= t && t < kfs[idx + 1].t {
                if kfs[idx].to != nil && kfs[idx].ti != nil { return (kfs[idx], kfs[idx + 1]) }
                return nil
            }
        }
        return nil
    }
}

/// Cubic-bezier easing (o/i keyframe control points), solved by bisection.
private func ltEase(_ o: [String: Any]?, _ i: [String: Any]?, _ u: Double) -> Double {
    guard let o = o, !o.isEmpty, let i = i, !i.isEmpty else { return u }
    func first(_ v: Any?, _ d: Double = 0) -> Double {
        if let arr = v as? [Any] { return arr.first.map { lNum($0) } ?? d }
        return lNum(v, d)
    }
    let x1 = first(o["x"]), y1 = first(o["y"])
    let x2 = first(i["x"]), y2 = first(i["y"])
    func bez(_ t: Double) -> Double {
        3 * (1 - t) * (1 - t) * t * x1 + 3 * (1 - t) * t * t * x2 + t * t * t
    }
    var lo = 0.0, hi = 1.0
    for _ in 0..<40 {
        let mid = (lo + hi) / 2
        if bez(mid) < u { lo = mid } else { hi = mid }
    }
    let t = (lo + hi) / 2
    return 3 * (1 - t) * (1 - t) * t * y1 + 3 * (1 - t) * t * t * y2 + t * t * t
}

/// Position along a segment; spatial tangents follow a 33-sample arc-length curve.
private func ltPosAt(_ a: LottieProp, _ t: Double) -> CGPoint {
    func plain() -> CGPoint {
        let v = a.at(t)
        return CGPoint(x: v.first ?? 0, y: v.count > 1 ? v[1] : 0)
    }
    guard let (ka, kb) = a.segAt(t),
          let s = ka.s, let e = ka.e ?? kb.s, s.count >= 2, e.count >= 2,
          let to = ka.to, let ti = ka.ti, to.count >= 2, ti.count >= 2 else {
        return plain()
    }
    let span = kb.t - ka.t
    let u = span != 0 ? (t - ka.t) / span : 1.0
    let g = ltEase(ka.o, ka.i, u)
    let p0 = CGPoint(x: s[0], y: s[1])
    let p3 = CGPoint(x: e[0], y: e[1])
    let p1 = CGPoint(x: s[0] + to[0], y: s[1] + to[1])
    let p2 = CGPoint(x: e[0] + ti[0], y: e[1] + ti[1])
    var pts: [CGPoint] = []
    var cum: [Double] = [0]
    for i in 0..<33 {
        let p = ltCubic(p0, p1, p2, p3, Double(i) / 32)
        pts.append(p)
        if i > 0 {
            cum.append(cum[i - 1] + hypot(p.x - pts[i - 1].x, p.y - pts[i - 1].y))
        }
    }
    let target = g * cum[32]
    for i in 1..<33 {
        if cum[i] >= target {
            let seg = (cum[i] - cum[i - 1]) != 0 ? (cum[i] - cum[i - 1]) : 1e-9
            let f = (target - cum[i - 1]) / seg
            return CGPoint(x: pts[i - 1].x + (pts[i].x - pts[i - 1].x) * f,
                           y: pts[i - 1].y + (pts[i].y - pts[i - 1].y) * f)
        }
    }
    return pts[32]
}

/// Layer position; supports split-dimension (p with separate x/y props).
private func ltLayerPos(_ ks: [String: Any], _ t: Double) -> CGPoint {
    guard let pd = lDict(ks["p"]) else {
        return ltPosAt(LottieProp(lDict(ks["p"])), t)
    }
    if pd["x"] != nil || pd["y"] != nil {
        let s0 = lNums(pd["s"]) ?? [0, 0]
        let x: Double
        if let xd = lDict(pd["x"]), !xd.isEmpty {
            x = LottieProp(xd).at(t).first ?? 0
        } else {
            x = s0.first ?? 0
        }
        let y: Double
        if let yd = lDict(pd["y"]), !yd.isEmpty {
            y = LottieProp(yd).at(t).first ?? 0
        } else {
            y = s0.count > 1 ? s0[1] : 0
        }
        return CGPoint(x: x, y: y)
    }
    return ltPosAt(LottieProp(pd), t)
}

// MARK: - Path flattening

struct LtContour {
    var pts: [CGPoint]
    var closed: Bool
}

/// Reference-typed on purpose: style snapshots capture these objects and later
/// trim/round items in the same layer mutate the shared instances (matches
/// lottie-web modifier order and the validated reference implementation).
final class PathState {
    var contours: [LtContour] = []
}

struct LottiePrim {
    let kind: String // "fl" or "st"
    let pts: [CGPoint]
    let rgb: (Double, Double, Double)
    let opacity: Double
    let width: Double
}

struct LtStyleRec {
    let kind: String
    let rgb: (Double, Double, Double)
    let opacity: Double
    let width: Double
    let states: [PathState]
}

private enum LtShapes {

    static func contourFromSh(_ ks: [String: Any]?, _ mat: LtMat) -> [LtContour] {
        guard let ks = ks else { return [] }
        var k: Any? = ks["k"]
        if let arr = lArr(k), let firstKf = arr.first as? [String: Any] { k = firstKf }
        guard let kd = lDict(k), let vAny = lArr(kd["v"]), !vAny.isEmpty else { return [] }
        let iAny = lArr(kd["i"]) ?? []
        let oAny = lArr(kd["o"]) ?? []
        let closed: Bool = {
            if let b = kd["c"] as? Bool { return b }
            return lNum(kd["c"]) != 0
        }()
        func pt(_ arr: [Any], _ i: Int) -> CGPoint {
            guard let v = arr[i] as? [Any], v.count >= 2 else { return .zero }
            return CGPoint(x: lNum(v[0]), y: lNum(v[1]))
        }
        let n = vAny.count
        var pts: [CGPoint] = []
        for i in 0..<n {
            let cur = pt(vAny, i)
            pts.append(cur)
            if i + 1 < n || closed {
                let j = (i + 1) % n
                let nxt = pt(vAny, j)
                let oi = i < oAny.count ? pt(oAny, i) : .zero
                let ij = j < iAny.count ? pt(iAny, j) : .zero
                let c1 = CGPoint(x: cur.x + oi.x, y: cur.y + oi.y)
                let c2 = CGPoint(x: nxt.x + ij.x, y: nxt.y + ij.y)
                for s in 1..<24 {
                    pts.append(ltCubic(cur, c1, c2, nxt, Double(s) / 24))
                }
            }
        }
        return [LtContour(pts: pts.map { mat.apply($0) }, closed: closed)]
    }

    static func contourFromRc(_ item: [String: Any], _ mat: LtMat, _ t: Double) -> [LtContour] {
        let p = LottieProp(lDict(item["p"])).at(t)
        let s = LottieProp(lDict(item["s"])).at(t)
        let r = LottieProp(lDict(item["r"])).at(t)
        let w = s.first ?? 0
        let h = s.count > 1 ? s[1] : 0
        let x0 = (p.first ?? 0) - w / 2
        let y0 = (p.count > 1 ? p[1] : 0) - h / 2
        let rr = max(0, min(r.first ?? 0, w / 2, h / 2))
        if rr <= 0.01 {
            let raw = [CGPoint(x: x0, y: y0), CGPoint(x: x0 + w, y: y0),
                       CGPoint(x: x0 + w, y: y0 + h), CGPoint(x: x0, y: y0 + h)]
            return [LtContour(pts: raw.map { mat.apply($0) }, closed: true)]
        }
        let corners = [(x0 + rr, y0), (x0 + w - rr, y0), (x0 + w, y0 + rr), (x0 + w, y0 + h - rr),
                       (x0 + w - rr, y0 + h), (x0 + rr, y0 + h), (x0, y0 + h - rr), (x0, y0 + rr)]
        let ctrs = [(x0 + w - rr, y0 + rr), (x0 + w - rr, y0 + h - rr),
                    (x0 + rr, y0 + h - rr), (x0 + rr, y0 + rr)]
        let starts = [0.0, 90.0, 180.0, 270.0]
        var pts: [CGPoint] = []
        for i in 0..<4 {
            for c in [corners[2 * i], corners[2 * i + 1]] {
                pts.append(CGPoint(x: c.0, y: c.1))
            }
            let cx = ctrs[i].0, cy = ctrs[i].1
            for sd in 1..<12 {
                let a = (starts[i] + Double(sd) * 90.0 / 12.0) * .pi / 180
                pts.append(CGPoint(x: cx + rr * cos(a), y: cy + rr * sin(a)))
            }
        }
        return [LtContour(pts: pts.map { mat.apply($0) }, closed: true)]
    }

    static func contourFromStar(_ item: [String: Any], _ mat: LtMat, _ t: Double) -> [LtContour] {
        let pt = LottieProp(lDict(item["pt"])).at(t).first ?? 0
        let p = LottieProp(lDict(item["p"])).at(t)
        let orr = LottieProp(lDict(item["or"])).at(t).first ?? 0
        let ir = item["ir"] != nil ? (LottieProp(lDict(item["ir"])).at(t).first ?? 0) : 0
        let rot = item["r"] != nil ? (LottieProp(lDict(item["r"])).at(t).first ?? 0) : 0
        let osF = item["os"] != nil ? (LottieProp(lDict(item["os"])).at(t).first ?? 0) / 100 : 0
        let isF = item["is"] != nil ? (LottieProp(lDict(item["is"])).at(t).first ?? 0) / 100 : 0
        let sy = lInt(item["sy"], 1)
        let n = Int(pt.rounded())
        guard n > 0 else { return [] }
        let step = 2 * .pi / Double(n)
        let px = p.first ?? 0
        let py = p.count > 1 ? p[1] : 0
        var verts: [(CGPoint, Double)] = []
        for i in 0..<n {
            let ao = rot * .pi / 180 - .pi / 2 + Double(i) * step
            verts.append((CGPoint(x: px + orr * cos(ao), y: py + orr * sin(ao)), osF))
            if sy == 1 {
                let ai = ao + step / 2
                verts.append((CGPoint(x: px + ir * cos(ai), y: py + ir * sin(ai)), isF))
            }
        }
        let m = verts.count
        guard m > 1 else { return [] }
        var pts: [CGPoint] = []
        for i in 0..<m {
            let (cur, pc) = verts[i]
            let nxt = verts[(i + 1) % m].0
            pts.append(cur)
            let c1 = CGPoint(x: cur.x + (nxt.x - cur.x) * pc, y: cur.y + (nxt.y - cur.y) * pc)
            let c2 = CGPoint(x: nxt.x + (cur.x - nxt.x) * pc, y: nxt.y + (cur.y - nxt.y) * pc)
            for s in 1..<16 {
                pts.append(ltCubic(cur, c1, c2, nxt, Double(s) / 16))
            }
        }
        return [LtContour(pts: pts.map { mat.apply($0) }, closed: true)]
    }

    /// Circular-approx fillet at every corner; tangent length r/tan(th/2), clamped.
    static func filletRadius(_ pts: [CGPoint], _ closed: Bool, _ radius: Double) -> [CGPoint] {
        let n = pts.count
        if n < 3 || radius <= 0.01 { return pts }
        var out: [CGPoint] = []
        let upper = closed ? n : n - 1
        guard upper > 0 else { return pts }
        for i in (closed ? 0..<n : 1..<max(n - 1, 1)) {
            let prev = pts[(i - 1 + n) % n]
            let cur = pts[i]
            let nxt = pts[(i + 1) % n]
            let lin = hypot(cur.x - prev.x, cur.y - prev.y)
            let lout = hypot(nxt.x - cur.x, nxt.y - cur.y)
            if lin < 0.01 || lout < 0.01 { out.append(cur); continue }
            let din = CGPoint(x: (cur.x - prev.x) / lin, y: (cur.y - prev.y) / lin)
            let dout = CGPoint(x: (nxt.x - cur.x) / lout, y: (nxt.y - cur.y) / lout)
            let dot = max(-1.0, min(1.0, din.x * dout.x + din.y * dout.y))
            let ang = acos(dot)
            if ang > .pi - 0.02 || ang < 0.02 { out.append(cur); continue }
            let d = min(radius / tan(ang / 2), 0.49 * lin, 0.49 * lout)
            let pIn = CGPoint(x: cur.x - din.x * d, y: cur.y - din.y * d)
            let pOut = CGPoint(x: cur.x + dout.x * d, y: cur.y + dout.y * d)
            out.append(pIn)
            let phi = .pi - ang
            let h = (4.0 / 3.0) * tan(phi / 4) * d
            let c1 = CGPoint(x: pIn.x + din.x * h, y: pIn.y + din.y * h)
            let c2 = CGPoint(x: pOut.x - dout.x * h, y: pOut.y - dout.y * h)
            for s in 1..<9 {
                out.append(ltCubic(pIn, c1, c2, pOut, Double(s) / 9))
            }
        }
        return out
    }

    static func polyLen(_ pts: [CGPoint], _ closed: Bool) -> Double {
        let n = pts.count
        let upper = closed ? n : n - 1
        guard upper > 0 else { return 0 }
        var length = 0.0
        for i in 0..<upper {
            let a = pts[i], b = pts[(i + 1) % n]
            length += hypot(b.x - a.x, b.y - a.y)
        }
        return length
    }

    /// Sub-path of [f0, f1] (fractions of total length) of one contour.
    static func slicePoly(_ pts: [CGPoint], _ closed: Bool, _ f0: Double, _ f1: Double) -> [CGPoint] {
        let length = polyLen(pts, closed)
        let n = pts.count
        if length < 1e-9 { return [] }
        let upper = closed ? n : n - 1
        guard upper > 0 else { return [] }
        var out: [CGPoint] = []
        let target = f0 * length
        var acc = 0.0
        var started = false
        for i in 0..<upper {
            let a = pts[i], b = pts[(i + 1) % n]
            let seg = hypot(b.x - a.x, b.y - a.y)
            let s0 = acc, s1 = acc + seg
            acc = s1
            if s1 < target || seg < 1e-9 { continue }
            if !started {
                let f = (target - s0) / seg
                out.append(CGPoint(x: a.x + (b.x - a.x) * f, y: a.y + (b.y - a.y) * f))
                started = true
            }
            if s1 <= f1 * length + 1e-9 {
                out.append(b)
            } else {
                let f = (f1 * length - s0) / seg
                out.append(CGPoint(x: a.x + (b.x - a.x) * f, y: a.y + (b.y - a.y) * f))
                break
            }
        }
        return out
    }

    /// Trim-path application (lottie-web TrimModifier math).
    static func applyTrim(_ state: PathState, _ sv: Double, _ ev: Double, _ ov: Double) {
        var off = fmod(ov, 360.0) / 360.0
        if off < 0 { off += 1 }
        func norm(_ v: Double) -> Double {
            if v > 1 { return 1 + off }
            if v < 0 { return 0 + off }
            return v + off
        }
        var u = norm(sv / 100.0)
        var f = norm(ev / 100.0)
        if u > f { swap(&u, &f) }
        u = (u * 10000).rounded() / 10000
        f = (f * 10000).rounded() / 10000
        if f == u { state.contours = []; return }
        if (f == 1 && u == 0) || (f == 0 && u == 1) { return }
        var newc: [LtContour] = []
        for c in state.contours {
            let ranges: [(Double, Double)]
            if f <= 1 {
                ranges = [(u, f)]
            } else if u >= 1 {
                ranges = [(u - 1, f - 1)]
            } else {
                ranges = [(u, 1), (0, f - 1)]
            }
            for (a0, b0) in ranges {
                let sl = slicePoly(c.pts, c.closed, a0, b0)
                if sl.count > 1 { newc.append(LtContour(pts: sl, closed: false)) }
            }
        }
        state.contours = newc
    }

    /// mm: merge all states' contours into the first state.
    static func mergeStates(_ states: [PathState]) {
        guard !states.isEmpty else { return }
        var allc: [LtContour] = []
        for st in states { allc.append(contentsOf: st.contours) }
        for st in states.dropFirst() { st.contours = [] }
        states[0].contours = allc
    }
}

// MARK: - Renderer

private enum LottieRenderer {

    static func render(_ doc: [String: Any], t: Double) -> [LottiePrim] {
        var assets: [String: [String: Any]] = [:]
        for a in lArr(doc["assets"]) ?? [] {
            guard let ad = lDict(a),
                  let layers = lArr(ad["layers"]), !layers.isEmpty,
                  let id = ad["id"] as? String else { continue }
            assets[id] = ad
        }
        var out: [LottiePrim] = []
        doComp(doc, tt: t, baseM: .identity, baseOp: 1.0, assets: assets, out: &out)
        return out
    }

    private static func doComp(_ comp: [String: Any], tt: Double, baseM: LtMat, baseOp: Double,
                               assets: [String: [String: Any]], out: inout [LottiePrim]) {
        let layers = (lArr(comp["layers"]) ?? []).compactMap { lDict($0) }
        guard !layers.isEmpty else { return }
        var byInd: [Int: [String: Any]] = [:]
        for l in layers { byInd[lInt(l["ind"], Int.min)] = l }

        func localM(_ layer: [String: Any], _ lt: Double) -> LtMat {
            let ks = lDict(layer["ks"]) ?? [:]
            return ltTrs(ltLayerPos(ks, lt),
                         LottieProp(lDict(ks["r"])).at(lt).first ?? 0,
                         LottieProp(lDict(ks["s"])).at(lt),
                         LottieProp(lDict(ks["a"])).at(lt))
        }
        func worldM(_ layer: [String: Any], _ lt: Double, _ depth: Int) -> LtMat {
            var m = localM(layer, lt)
            if depth < 16, let parent = byInd[lInt(layer["parent"], Int.min)] {
                m = worldM(parent, lt, depth + 1).mul(m)
            }
            return m
        }

        // First layer in the array paints on top → iterate reversed.
        for layer in layers.reversed() {
            if let hd = layer["hd"] as? Bool, hd { continue }
            let lt = tt
            let ip = lNum(layer["ip"], 0)
            let op = lNum(layer["op"], .infinity)
            guard ip <= lt && lt < op else { continue }
            let m = baseM.mul(worldM(layer, lt, 0))
            let ks = lDict(layer["ks"]) ?? [:]
            let o = (LottieProp(lDict(ks["o"])).at(lt).first ?? 0) / 100 * baseOp
            let ty = lInt(layer["ty"])
            if ty == 0 {
                guard let refId = layer["refId"] as? String,
                      let asset = assets[refId] else { continue }
                doComp(asset, tt: lt - lNum(layer["st"], 0), baseM: m, baseOp: o,
                       assets: assets, out: &out)
            } else if ty == 4 {
                drawLayer(layer, t: lt, m: m, op: o, out: &out)
            }
        }
    }

    private static func drawLayer(_ layer: [String: Any], t: Double, m: LtMat, op: Double,
                                  out: inout [LottiePrim]) {
        var styles: [LtStyleRec] = []
        _ = walk(lArr(layer["shapes"]) ?? [], m, op, t, &styles)
        // First-listed style paints on top; within a style, later shapes below.
        for st in styles.reversed() {
            for state in st.states.reversed() {
                for c in state.contours where c.pts.count >= 2 {
                    out.append(LottiePrim(kind: st.kind, pts: c.pts, rgb: st.rgb,
                                          opacity: st.opacity, width: st.width))
                }
            }
        }
    }

    private static func walk(_ items: [Any], _ matIn: LtMat, _ gopIn: Double, _ t: Double,
                             _ styles: inout [LtStyleRec]) -> [PathState] {
        var states: [PathState] = []
        var mat = matIn
        var gop = gopIn

        if let tr = (lArr(items) ?? []).compactMap({ lDict($0) }).first(where: { $0["ty"] as? String == "tr" }) {
            mat = mat.mul(ltTrs(ltLayerPos(tr, t),
                                LottieProp(lDict(tr["r"])).at(t).first ?? 0,
                                LottieProp(lDict(tr["s"])).at(t),
                                LottieProp(lDict(tr["a"])).at(t)))
            gop *= (LottieProp(lDict(tr["o"])).at(t).first ?? 0) / 100
        }

        for item in items {
            guard let it = lDict(item) else { continue }
            let ty = it["ty"] as? String
            switch ty {
            case "gr":
                states += walk(lArr(it["it"]) ?? [], mat, gop, t, &styles)
            case "sh":
                let ps = PathState()
                ps.contours = LtShapes.contourFromSh(lDict(it["ks"]), mat)
                states.append(ps)
            case "rc":
                let ps = PathState()
                ps.contours = LtShapes.contourFromRc(it, mat, t)
                states.append(ps)
            case "sr":
                let ps = PathState()
                ps.contours = LtShapes.contourFromStar(it, mat, t)
                states.append(ps)
            case "rd":
                let rr = LottieProp(lDict(it["r"])).at(t).first ?? 0
                for st in states {
                    st.contours = st.contours.map {
                        LtContour(pts: LtShapes.filletRadius($0.pts, $0.closed, rr), closed: $0.closed)
                    }
                }
            case "tm":
                let sv = LottieProp(lDict(it["s"])).at(t).first ?? 0
                let ev = LottieProp(lDict(it["e"])).at(t).first ?? 0
                let ov = LottieProp(lDict(it["o"])).at(t).first ?? 0
                for st in states { LtShapes.applyTrim(st, sv, ev, ov) }
            case "mm":
                if states.count > 1 { LtShapes.mergeStates(states) }
            case "fl", "st":
                let c = LottieProp(lDict(it["c"])).at(t)
                let oo = (LottieProp(lDict(it["o"])).at(t).first ?? 0) / 100 * gop
                let width: Double = ty == "st" ? (LottieProp(lDict(it["w"])).at(t).first ?? 0) : 0
                styles.append(LtStyleRec(kind: ty ?? "",
                                         rgb: (c.first ?? 0, c.count > 1 ? c[1] : 0, c.count > 2 ? c[2] : 0),
                                         opacity: oo, width: width, states: states))
            default:
                break
            }
        }
        return states
    }
}

// MARK: - .lottie archive reader (zip: manifest.json + animations/<id>.json)

enum LottieArchive {

    static func animationJSON(_ data: Data, prefer name: String?) -> [String: Any]? {
        guard let entries = readEntries(data) else { return nil }
        var anims: [(name: String, data: Data)] = []
        let prefix = "animations/"
        for (name, d) in entries where name.hasPrefix(prefix) && name.hasSuffix(".json") && name.count > prefix.count {
            anims.append((name, d))
        }
        guard !anims.isEmpty else { return nil }
        anims.sort { $0.name < $1.name }

        func strip(_ s: String) -> String { s.hasSuffix(".json") ? String(s.dropLast(5)) : s }

        if let wanted = name,
           let md = entries["manifest.json"],
           let m = (try? JSONSerialization.jsonObject(with: md)) as? [String: Any],
           let list = lArr(m["animations"]) {
            let recs = list.compactMap { lDict($0) }
            let pick = recs.first { ($0["id"] as? String).map(strip) == wanted }
                ?? recs.first { ($0["name"] as? String) == wanted }
            if let id = (pick?["id"] as? String).map(strip) {
                if let hit = anims.first(where: { strip(String($0.name.dropFirst(prefix.count))) == id }),
                   let obj = try? JSONSerialization.jsonObject(with: hit.data) as? [String: Any],
                   obj["layers"] != nil {
                    return obj
                }
            }
        }
        // Fallback: first parsable animation with layers.
        for a in anims {
            if let obj = try? JSONSerialization.jsonObject(with: a.data) as? [String: Any],
               obj["layers"] != nil {
                return obj
            }
        }
        return nil
    }

    /// Minimal zip reader: EOCD scan → central directory → local headers →
    /// stored (method 0) or raw-DEFLATE (method 8, COMPRESSION_ZLIB) payloads.
    private static func readEntries(_ data: Data) -> [String: Data]? {
        let bytes = [UInt8](data)
        guard bytes.count >= 22 else { return nil }
        var eocd = -1
        let floor = max(0, bytes.count - 22 - 65557)
        var i = bytes.count - 22
        while i >= floor {
            if bytes[i] == 0x50, bytes[i + 1] == 0x4B, bytes[i + 2] == 0x05, bytes[i + 3] == 0x06 {
                eocd = i
                break
            }
            i -= 1
        }
        guard eocd >= 0 else { return nil }
        func u16(_ off: Int) -> Int { Int(bytes[off]) | (Int(bytes[off + 1]) << 8) }
        func u32(_ off: Int) -> Int {
            Int(bytes[off]) | (Int(bytes[off + 1]) << 8) | (Int(bytes[off + 2]) << 16) | (Int(bytes[off + 3]) << 24)
        }
        let cdSize = u32(eocd + 12)
        let cdOffset = u32(eocd + 16)
        var entries: [String: Data] = [:]
        var pos = cdOffset
        let cdEnd = min(cdOffset + cdSize, bytes.count)
        while pos + 46 <= cdEnd {
            guard bytes[pos] == 0x50, bytes[pos + 1] == 0x4B, bytes[pos + 2] == 0x01, bytes[pos + 3] == 0x02 else { break }
            let method = u16(pos + 10)
            let csize = u32(pos + 20)
            let usize = u32(pos + 24)
            let nameLen = u16(pos + 28)
            let extraLen = u16(pos + 30)
            let commentLen = u16(pos + 32)
            let localOff = u32(pos + 42)
            guard pos + 46 + nameLen <= bytes.count else { break }
            let name = String(bytes: bytes[(pos + 46)..<(pos + 46 + nameLen)], encoding: .utf8) ?? ""

            if localOff + 30 <= bytes.count,
               bytes[localOff] == 0x50, bytes[localOff + 1] == 0x4B,
               bytes[localOff + 2] == 0x03, bytes[localOff + 3] == 0x04 {
                // Use the LOCAL header's name/extra lengths — they can differ
                // from the central-directory values.
                let lNameLen = u16(localOff + 26)
                let lExtraLen = u16(localOff + 28)
                let dataStart = localOff + 30 + lNameLen + lExtraLen
                let dataEnd = dataStart + csize
                if dataEnd <= bytes.count {
                    let raw = data.subdata(in: dataStart..<dataEnd)
                    var payload: Data?
                    if method == 0 {
                        payload = raw
                    } else if method == 8 {
                        payload = inflate(raw, usize)
                    }
                    if let p = payload, !name.isEmpty {
                        entries[name] = p
                    }
                }
            }
            pos += 46 + nameLen + extraLen + commentLen
        }
        return entries.isEmpty ? nil : entries
    }

    private static func inflate(_ src: Data, _ expected: Int) -> Data? {
        let cap = min(max(expected, 1), 256 * 1024 * 1024)
        var dst = Data(count: cap)
        let scratchSize = compression_decode_scratch_buffer_size(COMPRESSION_ZLIB)
        var scratch = Data(count: scratchSize)
        let decoded = dst.withUnsafeMutableBytes { (db: UnsafeMutableRawBufferPointer) -> Int in
            src.withUnsafeBytes { (sb: UnsafeRawBufferPointer) -> Int in
                scratch.withUnsafeMutableBytes { (sc: UnsafeMutableRawBufferPointer) -> Int in
                    guard let dPtr = db.bindMemory(to: UInt8.self).baseAddress,
                          let sPtr = sb.bindMemory(to: UInt8.self).baseAddress else { return 0 }
                    let scPtr = sc.bindMemory(to: UInt8.self).baseAddress
                    return compression_decode_buffer(dPtr, cap, sPtr, src.count,
                                                     scPtr, COMPRESSION_ZLIB)
                }
            }
        }
        guard decoded > 0 else { return nil }
        dst.removeSubrange(decoded..<dst.count)
        return dst
    }
}

// MARK: - Document

final class LottieDocument {
    let json: [String: Any]
    let width: Double
    let height: Double
    let fps: Double
    let firstFrame: Double
    let lastFrame: Double

    /// Play-once-and-hold: freeze one frame before the out-point.
    var holdFrame: Double { max(firstFrame, lastFrame - 1) }

    private init(json: [String: Any]) {
        self.json = json
        width = lNum(json["w"], 100)
        height = lNum(json["h"], 100)
        fps = max(lNum(json["fr"], 60), 1)
        firstFrame = lNum(json["ip"], 0)
        lastFrame = lNum(json["op"], 0)
    }

    private static var cache: [String: LottieDocument] = [:]

    static func named(_ name: String) -> LottieDocument? {
        if let hit = cache[name] { return hit }
        guard let url = Bundle.main.url(forResource: name, withExtension: "lottie"),
              let data = try? Data(contentsOf: url),
              let json = LottieArchive.animationJSON(data, prefer: name) else { return nil }
        let doc = LottieDocument(json: json)
        cache[name] = doc
        return doc
    }
}

// MARK: - View

/// Renders a LottieDocument with CAShapeLayers. Plays once (ip → op−1) and
/// holds the final frame; playOnce() restarts from the first frame. The view's
/// layer content is in standard bottom-left coords, so the fit matrix flips
/// Lottie's top-left space into it.
final class LottieView: NSView {

    private let document: LottieDocument
    private var tint: NSColor?
    private var currentFrame: Double
    private var displayLink: CADisplayLink?
    private var playStart: CFAbsoluteTime = 0
    private var startFrame: Double = 0

    init(document: LottieDocument, tint: NSColor? = nil) {
        self.document = document
        self.tint = tint
        self.currentFrame = document.holdFrame
        super.init(frame: .zero)
        wantsLayer = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    deinit { displayLink?.invalidate() }

    func setTint(_ color: NSColor?) {
        guard tint != color else { return }
        tint = color
        rebuild()
    }

    /// Play the animation once from its first frame, then hold.
    func playOnce() {
        displayLink?.invalidate()
        displayLink = nil
        guard Theme.motionEnabled else {
            currentFrame = document.holdFrame
            rebuild()
            return
        }
        currentFrame = document.firstFrame
        startFrame = document.firstFrame
        playStart = CFAbsoluteTimeGetCurrent()
        rebuild()
        // macOS CADisplayLink is created through NSScreen (macOS 14+).
        guard let screen = window?.screen ?? NSScreen.main else {
            currentFrame = document.holdFrame
            rebuild()
            return
        }
        let link = screen.displayLink(target: self, selector: #selector(step))
        link.add(to: .main, forMode: .common)
        displayLink = link
    }

    @objc private func step() {
        guard Theme.motionEnabled else {
            displayLink?.invalidate()
            displayLink = nil
            currentFrame = document.holdFrame
            rebuild()
            return
        }
        let frame = startFrame + (CFAbsoluteTimeGetCurrent() - playStart) * document.fps
        if frame >= document.holdFrame {
            displayLink?.invalidate()
            displayLink = nil
            currentFrame = document.holdFrame
        } else {
            currentFrame = frame
        }
        rebuild()
    }

    override func layout() {
        super.layout()
        rebuild()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        rebuild()
    }

    private func rebuild() {
        guard let host = layer, bounds.width > 0, bounds.height > 0 else { return }
        let prims = LottieRenderer.render(document.json, t: currentFrame)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        host.sublayers?.forEach { $0.removeFromSuperlayer() }

        let bw = bounds.width, bh = bounds.height
        let s = min(bw / document.width, bh / document.height)
        if s.isFinite && s > 0 {
            let ox = (bw - s * document.width) / 2
            let oy = (bh - s * document.height) / 2
            // Flip Lottie's top-left y-down space first, then scale, then center.
            let fit = LtMat.translate(ox, oy)
                .mul(LtMat.scale(s))
                .mul(LtMat(a: 1, b: 0, c: 0, d: -1, e: 0, f: document.height))

            for prim in prims {
                let path = CGMutablePath()
                path.move(to: fit.apply(prim.pts[0]))
                for i in 1..<prim.pts.count {
                    path.addLine(to: fit.apply(prim.pts[i]))
                }
                if prim.kind == "fl" { path.closeSubpath() }

                let alpha = min(max(prim.opacity, 0), 1)
                let color: CGColor
                if let tint = tint {
                    color = Theme.cgColor(of: tint.withAlphaComponent(alpha),
                                          for: effectiveAppearance)
                } else {
                    color = NSColor(srgbRed: prim.rgb.0, green: prim.rgb.1,
                                    blue: prim.rgb.2, alpha: alpha).cgColor
                }

                let shape = CAShapeLayer()
                shape.path = path
                if prim.kind == "fl" {
                    shape.fillColor = color
                    shape.strokeColor = nil
                } else {
                    shape.fillColor = nil
                    shape.strokeColor = color
                    shape.lineWidth = CGFloat(prim.width * s)
                    shape.lineCap = .round
                    shape.lineJoin = .round
                }
                host.addSublayer(shape)
            }
        }
        CATransaction.commit()
    }
}
