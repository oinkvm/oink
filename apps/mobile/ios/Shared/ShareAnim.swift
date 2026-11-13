import SwiftUI

// Share card stage maths (share-anim SPEC.zh-CN.md §5), kept free of the palette so the host tests check it.

struct ShareAnimFrame { var x: CGFloat; var y: CGFloat; var angle: CGFloat; var scale: CGFloat; var opacity: CGFloat; var trail: [CGPoint]; var nudge: CGFloat }

/// Where the arrow lands (share-anim SPEC §3.6, §5.8), chosen by the receiving device's kind for
/// the whole card: a computer (macOS / Windows) or a phone (iPhone, iPad, Android).
enum ShareTarget {
    case computer, phone
    /// Flight end E: the arrow's tail point lands here.
    var end: CGPoint { self == .phone ? CGPoint(x: 199, y: 50.5) : CGPoint(x: 195, y: 49) }
    var nudgeCenter: CGPoint { self == .phone ? CGPoint(x: 204, y: 53) : CGPoint(x: 200, y: 54) }
    /// Origin (Wi-Fi dot centre) of the offline mark while waiting (§3.8).
    var offlineOrigin: CGPoint { self == .phone ? CGPoint(x: 204, y: 58.5) : CGPoint(x: 200, y: 57) }
}

/// What the hand-over draws `ms` after the target came online (SPEC §5.9); `t` nil: no arrow yet.
struct ShareHandoff: Equatable { var offline: CGFloat; var logo: CGFloat; var bar: CGFloat; var t: CGFloat? }

/// The stage maths (SPEC §5, `preview.html` `<script id="core">`) as pure functions.
enum ShareAnim {
    static let duration: CGFloat = 1400
    static let handoffMs: CGFloat = 300, leadMs: CGFloat = 160, enterWaitMs: CGFloat = 150
    static let start = CGPoint(x: 54, y: 50), control = CGPoint(x: 116, y: 4)
    static func clamp(_ x: CGFloat) -> CGFloat { min(1, max(0, x)) }
    static func seg(_ x: CGFloat, _ a: CGFloat, _ b: CGFloat) -> CGFloat { clamp((x - a) / (b - a)) }
    static func bezier(_ x: CGFloat, _ c1: CGPoint, _ c2: CGPoint) -> CGFloat {
        if x <= 0 { return 0 }; if x >= 1 { return 1 }
        func cx(_ t: CGFloat) -> CGFloat { 3*c1.x*t*(1-t)*(1-t) + 3*c2.x*t*t*(1-t) + t*t*t }
        func cy(_ t: CGFloat) -> CGFloat { 3*c1.y*t*(1-t)*(1-t) + 3*c2.y*t*t*(1-t) + t*t*t }
        func dx(_ t: CGFloat) -> CGFloat { 3*c1.x*(1-t)*(1-t) + 6*(c2.x-c1.x)*t*(1-t) + 3*(1-c2.x)*t*t }
        var t = x
        for _ in 0..<6 { let e = cx(t)-x, d = dx(t); if abs(e)<0.00001 { return cy(t) }; if abs(d)<0.000001 { break }; t -= e/d }
        var lo: CGFloat = 0, hi: CGFloat = 1; t = x
        for _ in 0..<20 { let v = cx(t); if abs(v-x)<0.00001 { break }; if v<x { lo=t } else { hi=t }; t=(lo+hi)/2 }
        return cy(t)
    }
    static func easeIn(_ x: CGFloat) -> CGFloat { bezier(x, CGPoint(x: 0, y: 0), CGPoint(x: 0.2, y: 1)) }
    static func easeOut(_ x: CGFloat) -> CGFloat { bezier(x, CGPoint(x: 0.4, y: 0), CGPoint(x: 1, y: 1)) }
    /// VI `--ease`: the indeterminate bar and entering the wait.
    static func easeStandard(_ x: CGFloat) -> CGFloat { bezier(x, CGPoint(x: 0.2, y: 0), CGPoint(x: 0, y: 1)) }
    static func point(_ u: CGFloat, _ end: CGPoint) -> CGPoint { let m=1-u; return CGPoint(x:m*m*start.x+2*m*u*control.x+u*u*end.x, y:m*m*start.y+2*m*u*control.y+u*u*end.y) }
    static func tangent(_ u: CGFloat, _ end: CGPoint) -> CGPoint { CGPoint(x:2*(1-u)*(control.x-start.x)+2*u*(end.x-control.x), y:2*(1-u)*(control.y-start.y)+2*u*(end.y-control.y)) }
    static func u(_ t: CGFloat) -> CGFloat { bezier(clamp(t/0.72), CGPoint(x:0.35,y:0), CGPoint(x:0.25,y:1)) }
    /// One frame at normalised time `t` of a 1400 ms round (SPEC §5.3).
    static func frame(_ t: CGFloat, _ target: ShareTarget = .computer) -> ShareAnimFrame {
        let end = target.end
        let flight=u(t), p=point(flight, end), d=tangent(flight, end)
        let fi=easeIn(seg(t,0,0.10)), fo=easeOut(seg(t,0.58,0.74))
        let sh=bezier(seg(t,0.30,0.60), CGPoint(x:0.4,y:0), CGPoint(x:0.2,y:1))
        var trail:[CGPoint]=[]; for k in 0...12 { trail.append(point(u(max(0,t-CGFloat(k)*0.16/12)), end)) }
        let n=seg(t,0.62,0.86)
        return ShareAnimFrame(x:p.x,y:p.y,angle:atan2(d.y,d.x),scale:(0.6+0.4*fi)*(1-0.45*sh),opacity:fi*(1-fo),trail:trail,nudge:1+0.05*sin(.pi*n))
    }
    /// Reduced-motion pose: u = 0.5, the top of the arc (SPEC §7).
    static let staticT: CGFloat = { var lo: CGFloat = 0, hi: CGFloat = 0.72; for _ in 0..<40 { let m = (lo + hi) / 2; if u(m) < 0.5 { lo = m } else { hi = m } }; return (lo + hi) / 2 }()
    /// The hand-over `ms` after the target came online (SPEC §5.9 `handoff(ms)`).
    static func handoff(_ ms: CGFloat) -> ShareHandoff {
        ShareHandoff(offline: 1 - easeOut(seg(ms, 0, leadMs)), logo: 1 - easeIn(seg(ms, leadMs, handoffMs)), bar: seg(ms, 0, 150),
                     t: ms < leadMs ? nil : (ms - leadMs).truncatingRemainder(dividingBy: duration) / duration)
    }
    /// Entering the wait `ms` after it began: the marks' opacity; the arrow's is 1 − this.
    static func enterWait(_ ms: CGFloat) -> CGFloat { easeStandard(seg(ms, 0, enterWaitMs)) }
    static func trail(_ points:[CGPoint]) -> (Path, CGPoint, CGPoint)? {
        let length=zip(points.dropFirst(),points).reduce(CGFloat.zero){$0+hypot($1.0.x-$1.1.x,$1.0.y-$1.1.y)}; guard length>=0.5 else{return nil}
        let last=points.count-1; var left:[CGPoint]=[], right:[CGPoint]=[]
        for i in 0...last { let a=points[max(0,i-1)], b=points[min(last,i+1)], tx=a.x-b.x, ty=a.y-b.y, m=max(1,hypot(tx,ty)), h=(3.3+(1-3.3)*CGFloat(i)/CGFloat(last))/2, n=CGPoint(x:-ty/m,y:tx/m); left.append(CGPoint(x:points[i].x+n.x*h,y:points[i].y+n.y*h)); right.append(CGPoint(x:points[i].x-n.x*h,y:points[i].y-n.y*h)) }
        var path=Path(); path.move(to:left[0]); left.dropFirst().forEach{path.addLine(to:$0)}; right.reversed().forEach{path.addLine(to:$0)}; path.closeSubpath(); return (path,points[0],points[last])
    }
    static func trailLength(_ points: [CGPoint]) -> CGFloat { zip(points.dropFirst(), points).reduce(CGFloat.zero) { $0 + hypot($1.0.x - $1.1.x, $1.0.y - $1.1.y) } }
    static func arrow() -> Path { var p=Path(); p.move(to:CGPoint(x:18.48,y:0)); p.addLine(to:CGPoint(x:0.31,y:-7.87)); p.addLine(to:CGPoint(x:-0.31,y:7.87)); p.addQuadCurve(to:CGPoint(x:18.48,y:0),control:CGPoint(x:9.78,y:5.71)); p.closeSubpath(); return p }
    static func phone() -> Path { var p=Path(roundedRect:CGRect(x:23,y:30,width:26,height:46),cornerRadius:5.5); p.move(to:CGPoint(x:32.5,y:35.5));p.addLine(to:CGPoint(x:39.5,y:35.5));p.move(to:CGPoint(x:32,y:70.5));p.addLine(to:CGPoint(x:40,y:70.5));return p }
    static func computer() -> Path { var p=Path(roundedRect:CGRect(x:177,y:36,width:46,height:31),cornerRadius:3);p.move(to:CGPoint(x:169,y:72));p.addLine(to:CGPoint(x:231,y:72));return p }
    /// The receiving phone: this phone mirrored about the stage's centre line x = 120 (SPEC §3.6).
    static func farPhone() -> Path { var p=Path(roundedRect:CGRect(x:191,y:30,width:26,height:46),cornerRadius:5.5); p.move(to:CGPoint(x:200.5,y:35.5));p.addLine(to:CGPoint(x:207.5,y:35.5)); p.move(to:CGPoint(x:200,y:70.5));p.addLine(to:CGPoint(x:208,y:70.5));return p }
    /// The offline mark's arcs around its origin (SPEC §3.8): r 10 and r 5.5, ±45° about straight up.
    static func offlineArcs() -> Path {
        var p = Path()
        p.addArc(center: .zero, radius: 10, startAngle: .degrees(-135), endAngle: .degrees(-45), clockwise: false)
        p.move(to: CGPoint(x: -3.89, y: -3.89))
        p.addArc(center: .zero, radius: 5.5, startAngle: .degrees(-135), endAngle: .degrees(-45), clockwise: false)
        return p
    }
    static func offlineSlash() -> Path { var p = Path(); p.move(to: CGPoint(x: -7, y: -12.5)); p.addLine(to: CGPoint(x: 7, y: 1.5)); return p }
}

