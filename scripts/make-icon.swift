// Renders the DuckPlus app icon: swift script/make-icon.swift assets/icon/duckplus-1024.png
import AppKit

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "duckplus-1024.png"
let s: CGFloat = 1024
let img = NSImage(size: NSSize(width: s, height: s))
img.lockFocus()
let ctx = NSGraphicsContext.current!.cgContext

// macOS icon grid: 824pt body centered in 1024 with a soft shadow.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: NSColor.black.withAlphaComponent(0.35).cgColor)
let bodyPath = CGPath(roundedRect: body, cornerWidth: 186, cornerHeight: 186, transform: nil)
ctx.addPath(bodyPath)
ctx.setFillColor(NSColor(red: 0.06, green: 0.063, blue: 0.08, alpha: 1).cgColor)
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(bodyPath)
ctx.clip()
let space = CGColorSpaceCreateDeviceRGB()
let bg = CGGradient(colorsSpace: space, colors: [
    NSColor(red: 0.13, green: 0.135, blue: 0.16, alpha: 1).cgColor,
    NSColor(red: 0.045, green: 0.05, blue: 0.063, alpha: 1).cgColor,
] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(bg, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
ctx.restoreGState()

// The mark: a duck-yellow tile with a dark eye, same as the in-app logo.
let mark = CGRect(x: 292, y: 292, width: 440, height: 440)
let markPath = CGPath(roundedRect: mark, cornerWidth: 132, cornerHeight: 132, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -8), blur: 40, color: NSColor(red: 1, green: 0.83, blue: 0.23, alpha: 0.35).cgColor)
ctx.addPath(markPath)
ctx.setFillColor(NSColor(red: 1, green: 0.83, blue: 0.23, alpha: 1).cgColor)
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(markPath)
ctx.clip()
let sheen = CGGradient(colorsSpace: space, colors: [
    NSColor(white: 1, alpha: 0.28).cgColor, NSColor(white: 1, alpha: 0).cgColor,
] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(sheen, start: CGPoint(x: 512, y: 732), end: CGPoint(x: 512, y: 512), options: [])
ctx.restoreGState()
ctx.setFillColor(NSColor(red: 0.09, green: 0.08, blue: 0.04, alpha: 1).cgColor)
ctx.fillEllipse(in: CGRect(x: 437, y: 437, width: 150, height: 150))

img.unlockFocus()
let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
