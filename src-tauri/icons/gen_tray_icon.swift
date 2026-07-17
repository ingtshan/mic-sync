// 渲染 SF Symbol "mic" 为菜单栏模板图标 PNG(黑色 + alpha)
// 用法: swift gen_tray_icon.swift <输出路径> <像素尺寸> [--badge]
// --badge: 右上角加一个实心圆点徽标(周围留一圈透明间隔,深浅色菜单栏都读得清),
//          用作「有待确认请求」的注意态托盘图标 tray-attention.png
import AppKit

let out = CommandLine.arguments[1]
let px = Int(CommandLine.arguments[2])!
let badge = CommandLine.arguments.contains("--badge")

guard let base = NSImage(systemSymbolName: "mic", accessibilityDescription: nil),
      let symbol = base.withSymbolConfiguration(.init(pointSize: CGFloat(px), weight: .regular))
else { fatalError("无法加载 SF Symbol mic") }

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
rep.size = NSSize(width: px, height: px)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high

// 等比缩放居中(SF Symbol 本身不是正方形)
let s = symbol.size
let scale = min(CGFloat(px) / s.width, CGFloat(px) / s.height)
let w = s.width * scale, h = s.height * scale
let rect = NSRect(x: (CGFloat(px) - w) / 2, y: (CGFloat(px) - h) / 2, width: w, height: h)
// 模板符号直接绘制即为纯黑形状,保留 alpha 抗锯齿
symbol.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1.0)

if badge {
    let r = CGFloat(px) * 0.18 // 圆点半径
    let gap = CGFloat(px) * 0.07 // 与麦克风图形之间的透明间隔环
    let cx = CGFloat(px) - r - 1, cy = CGFloat(px) - r - 1 // AppKit 原点在左下,右上角
    // 先把徽标周围一圈抠成透明(在麦克风图形上挖出缺口),圆点才不会糊成一团
    if let cg = NSGraphicsContext.current?.cgContext {
        cg.setBlendMode(.clear)
        let hole = NSBezierPath(ovalIn: NSRect(
            x: cx - r - gap, y: cy - r - gap, width: (r + gap) * 2, height: (r + gap) * 2))
        hole.fill()
        cg.setBlendMode(.normal)
    }
    NSColor.black.setFill()
    NSBezierPath(ovalIn: NSRect(x: cx - r, y: cy - r, width: r * 2, height: r * 2)).fill()
}
NSGraphicsContext.restoreGraphicsState()

let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: out))
print("written \(out) \(px)x\(px)\(badge ? " badge" : "")")
