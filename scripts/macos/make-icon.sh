#!/usr/bin/env bash
# Generates ui/MediaLake.Mac/Resources/AppIcon.icns (rounded accent square + two stacked waves), the same
# design as scripts/windows/make-icon.ps1. Needs Xcode (swift, iconutil).
#
# NOT YET RUN: written against the layout in plan2.md; ui/MediaLake.Mac does not exist yet.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$root/ui/MediaLake.Mac/Resources/AppIcon.icns"
work="$(mktemp -d)"
iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"

cat > "$work/draw.swift" <<'SWIFT'
import AppKit

// macOS icons leave a margin around the shape; AppKit's y axis points up (GDI's points down).
func png(_ size: Int) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4,
                               hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let pad = CGFloat(size) * 0.1, side = CGFloat(size) - 2 * pad
    let square = NSBezierPath(roundedRect: NSRect(x: pad, y: pad, width: side, height: side), xRadius: side * 0.22, yRadius: side * 0.22)
    NSGradient(starting: NSColor(srgbRed: 0, green: 120 / 255, blue: 215 / 255, alpha: 1),
               ending: NSColor(srgbRed: 88 / 255, green: 60 / 255, blue: 220 / 255, alpha: 1))!.draw(in: square, angle: -45)
    NSColor.white.setStroke()
    // Two tildes stacked vertically, each one a single S-curve spanning the middle of the square.
    let x0 = pad + side * 0.2, x1 = pad + side * 0.8, w = x1 - x0, amp = side * 0.075
    for cy in [pad + side * 0.36, pad + side * 0.64] {
        let wave = NSBezierPath()
        wave.lineWidth = max(1.5, side * 0.1)
        wave.lineCapStyle = .round
        wave.move(to: NSPoint(x: x0, y: cy - amp))
        wave.curve(to: NSPoint(x: x1, y: cy + amp),
                   controlPoint1: NSPoint(x: x0 + w / 3, y: cy + amp * 1.33),
                   controlPoint2: NSPoint(x: x0 + 2 * w / 3, y: cy - amp * 1.33))
        wave.stroke()
    }
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let dir = CommandLine.arguments[1]
for base in [16, 32, 128, 256, 512] {
    try png(base).write(to: URL(fileURLWithPath: "\(dir)/icon_\(base)x\(base).png"))
    try png(base * 2).write(to: URL(fileURLWithPath: "\(dir)/icon_\(base)x\(base)@2x.png"))
}
SWIFT

swift "$work/draw.swift" "$iconset"
mkdir -p "$(dirname "$out")"
iconutil -c icns "$iconset" -o "$out"
rm -rf "$work"
echo "wrote $out"
