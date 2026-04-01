#!/usr/bin/env swift

import AppKit
import Foundation

let appRoot = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
let sourceURL = appRoot.appendingPathComponent("public/iris-logo.png")
let outputURL = appRoot.appendingPathComponent("src-tauri/icons/bundle-icon.png")

let canvasSize = CGSize(width: 1024, height: 1024)
let tileInset: CGFloat = 36
let tileCornerRadius: CGFloat = 220
let markSize: CGFloat = 780

func color(_ rgb: Int, alpha: CGFloat = 1) -> NSColor {
  let red = CGFloat((rgb >> 16) & 0xff) / 255
  let green = CGFloat((rgb >> 8) & 0xff) / 255
  let blue = CGFloat(rgb & 0xff) / 255
  return NSColor(calibratedRed: red, green: green, blue: blue, alpha: alpha)
}

guard let mark = NSImage(contentsOf: sourceURL) else {
  fputs("Failed to load source logo at \(sourceURL.path)\n", stderr)
  exit(1)
}

guard let rep = NSBitmapImageRep(
  bitmapDataPlanes: nil,
  pixelsWide: Int(canvasSize.width),
  pixelsHigh: Int(canvasSize.height),
  bitsPerSample: 8,
  samplesPerPixel: 4,
  hasAlpha: true,
  isPlanar: false,
  colorSpaceName: .deviceRGB,
  bytesPerRow: 0,
  bitsPerPixel: 0
) else {
  fputs("Failed to allocate bitmap buffer\n", stderr)
  exit(1)
}

rep.size = NSSize(width: canvasSize.width, height: canvasSize.height)

guard let context = NSGraphicsContext(bitmapImageRep: rep) else {
  fputs("Failed to create drawing context\n", stderr)
  exit(1)
}

let tileRect = CGRect(
  x: tileInset,
  y: tileInset,
  width: canvasSize.width - (tileInset * 2),
  height: canvasSize.height - (tileInset * 2)
)
let markRect = CGRect(
  x: (canvasSize.width - markSize) / 2,
  y: (canvasSize.height - markSize) / 2,
  width: markSize,
  height: markSize
)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
context.imageInterpolation = .high

NSColor.clear.setFill()
NSBezierPath(rect: CGRect(origin: .zero, size: canvasSize)).fill()

let tilePath = NSBezierPath(roundedRect: tileRect, xRadius: tileCornerRadius, yRadius: tileCornerRadius)

let tileShadow = NSShadow()
tileShadow.shadowColor = color(0x000000, alpha: 0.38)
tileShadow.shadowBlurRadius = 34
tileShadow.shadowOffset = NSSize(width: 0, height: -16)
tileShadow.set()
color(0x060606).setFill()
tilePath.fill()

NSGraphicsContext.restoreGraphicsState()
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
tilePath.addClip()

NSGradient(colors: [color(0x171717), color(0x020202)])?.draw(in: tilePath, angle: 90)

let ambientGlow = NSGradient(colors: [
  color(0x8B38FF, alpha: 0.15),
  color(0x451175, alpha: 0.08),
  color(0x000000, alpha: 0.0),
])
ambientGlow?.draw(
  fromCenter: CGPoint(x: tileRect.midX, y: tileRect.midY + 18),
  radius: 0,
  toCenter: CGPoint(x: tileRect.midX, y: tileRect.midY + 18),
  radius: tileRect.width * 0.48,
  options: []
)

NSGradient(colors: [color(0xffffff, alpha: 0.16), color(0xffffff, alpha: 0.03), color(0xffffff, alpha: 0.0)])?
  .draw(in: CGRect(x: tileRect.minX, y: tileRect.midY, width: tileRect.width, height: tileRect.height / 2), angle: 90)

color(0xffffff, alpha: 0.09).setStroke()
tilePath.lineWidth = 2
tilePath.stroke()

let markGlow = NSShadow()
markGlow.shadowColor = color(0xA94EFF, alpha: 0.22)
markGlow.shadowBlurRadius = 22
markGlow.shadowOffset = .zero
markGlow.set()
mark.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: 1)

NSGraphicsContext.restoreGraphicsState()
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
tilePath.addClip()
mark.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: 1)
NSGraphicsContext.restoreGraphicsState()

guard let png = rep.representation(using: .png, properties: [:]) else {
  fputs("Failed to encode output PNG\n", stderr)
  exit(1)
}

try png.write(to: outputURL, options: .atomic)
