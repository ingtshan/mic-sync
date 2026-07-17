// 渲染 SF Symbol "mic" 为菜单栏模板图标 PNG(黑色 + alpha)
// 用法: swift gen_tray_icon.swift <输出路径> <像素尺寸>
import AppKit

let out = CommandLine.arguments[1]
let px = Int(CommandLine.arguments[2])!

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
NSGraphicsContext.restoreGraphicsState()

let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: out))
print("written \(out) \(px)x\(px)")
