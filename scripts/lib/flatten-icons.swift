// Copy PNGs onto an opaque white background, dropping the alpha channel.
//
// App Store Connect rejects an upload outright when an icon in the asset
// catalog carries one — "Invalid large app icon ... can't be transparent or
// contain an alpha channel" (error 90717) — and `tauri icon` produces the iOS
// set with alpha. White because that is already the background the set is
// drawn against (`scripts/build-brand-assets.py`), so nothing changes visually.
//
// CoreGraphics rather than a converter: it is lossless and needs nothing
// installed beyond the Xcode that packaging already requires. ImageMagick
// would do it in one line and is not dependable — the copy on the machine this
// was written on could not load its own liblqr.
//
// Usage: flatten-icons.swift <dest-dir> <src.png>...
// Takes every file in one invocation because `swift` compiles the script on
// each run, and eighteen compiles to copy eighteen icons is most of a minute.
import AppKit
import Foundation

let args = CommandLine.arguments
guard args.count >= 3 else {
  FileHandle.standardError.write("usage: flatten-icons.swift <dest-dir> <src.png>...\n".data(using: .utf8)!)
  exit(2)
}
let destDir = URL(fileURLWithPath: args[1], isDirectory: true)

func fail(_ message: String) -> Never {
  FileHandle.standardError.write("flatten-icons: \(message)\n".data(using: .utf8)!)
  exit(1)
}

for path in args.dropFirst(2) {
  guard let image = NSImage(contentsOfFile: path),
        let tiff = image.tiffRepresentation,
        let source = NSBitmapImageRep(data: tiff),
        let cgImage = source.cgImage
  else { fail("cannot read \(path)") }

  let width = source.pixelsWide, height = source.pixelsHigh
  // `noneSkipLast` is what makes the result have no alpha channel at all,
  // rather than a fully opaque one — which Apple rejects just the same.
  guard let context = CGContext(
    data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpaceCreateDeviceRGB(),
    bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue
  ) else { fail("cannot make a context for \(path)") }

  context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
  context.fill(CGRect(x: 0, y: 0, width: width, height: height))
  context.draw(cgImage, in: CGRect(x: 0, y: 0, width: width, height: height))

  guard let flattened = context.makeImage(),
        let data = NSBitmapImageRep(cgImage: flattened).representation(using: .png, properties: [:])
  else { fail("cannot encode \(path)") }

  let dest = destDir.appendingPathComponent((path as NSString).lastPathComponent)
  do { try data.write(to: dest) } catch { fail("cannot write \(dest.path): \(error)") }
}
