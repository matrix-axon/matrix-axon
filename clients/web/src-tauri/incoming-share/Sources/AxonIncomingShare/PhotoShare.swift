import Foundation
import ImageIO
import UniformTypeIdentifiers

/// Original bytes stay intact; ImageIO decodes only bounded representations.
public enum PhotoShare {
  public struct Output {
    public let data: Data
    public let mime: String
    public let fileExtension: String
    public func filename(_ original: String) -> String {
      PhotoShare.filename(original, extension: fileExtension, replacing: true)
    }
  }

  private static func dimensions(_ source: CGImageSource) throws -> (Int, Int) {
    guard
      let properties = CGImageSourceCopyPropertiesAtIndex(
        source, CGImageSourceGetPrimaryImageIndex(source), nil) as? [CFString: Any],
      let width = properties[kCGImagePropertyPixelWidth] as? Int,
      let height = properties[kCGImagePropertyPixelHeight] as? Int,
      width > 0, height > 0, width <= 65535, height <= 65535,
      width <= 200_000_000 / height
    else { throw ShareFailure.invalid }
    return (width, height)
  }
  private static func source(_ url: URL) throws -> CGImageSource {
    guard
      let source = CGImageSourceCreateWithURL(
        url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
      CGImageSourceGetCount(source) > 0
    else { throw ShareFailure.invalid }
    _ = try dimensions(source)
    return source
  }
  private static func detectedMime(_ source: CGImageSource) throws -> String {
    guard let identifier = CGImageSourceGetType(source),
      let type = UTType(identifier as String), type.conforms(to: .image),
      let mime = type.preferredMIMEType, mime.hasPrefix("image/"), mime != "image/svg+xml"
    else { throw ShareFailure.invalid }
    return mime
  }
  public static func mime(_ url: URL) throws -> String { try inspect(url).mime }
  public static func inspect(_ url: URL) throws -> (mime: String, thumbnail: Data) {
    let source = try source(url)
    return (try detectedMime(source), try encode(
      image(source, pixels: 480), type: .jpeg, limit: ShareLimits.thumbnailBytes,
      cancellation: ShareCancellation()))
  }
  public static func originalFilename(_ name: String, mime: String) -> String {
    filename(name, extension: UTType(mimeType: mime)?.preferredFilenameExtension ?? "img", replacing: false)
  }
  private static func filename(_ original: String, extension suffix: String, replacing: Bool) -> String {
    let url = URL(fileURLWithPath: original)
    if !replacing && !url.pathExtension.isEmpty { return original }
    let stem = replacing ? url.deletingPathExtension().lastPathComponent : original
    var bounded = ""
    var bytes = 0
    for scalar in stem.unicodeScalars {
      guard bytes + scalar.utf8.count <= 254 - suffix.utf8.count else { break }
      bounded.unicodeScalars.append(scalar)
      bytes += scalar.utf8.count
    }
    return (bounded.isEmpty ? "photo" : bounded) + "." + suffix
  }
  private static func image(_ source: CGImageSource, pixels: Int) throws -> CGImage {
    guard
      let image = CGImageSourceCreateThumbnailAtIndex(
        source, CGImageSourceGetPrimaryImageIndex(source),
        [
          kCGImageSourceCreateThumbnailFromImageAlways: true,
          kCGImageSourceThumbnailMaxPixelSize: pixels,
          kCGImageSourceCreateThumbnailWithTransform: true,
          // Cache the bounded thumbnail, never a full-resolution raster.
          kCGImageSourceShouldCacheImmediately: true,
          kCGImageSourceShouldAllowFloat: false,
        ] as CFDictionary)
    else { throw ShareFailure.invalid }
    return image
  }

  /// Stop the encoder before it can append beyond the byte budget.
  private final class Sink {
    let cancellation: ShareCancellation
    let limit: Int
    private let lock = NSLock()
    private var data = Data()
    private var exceeded = false
    init(limit: Int, cancellation: ShareCancellation) {
      self.limit = limit
      self.cancellation = cancellation
    }
    func append(_ pointer: UnsafeRawPointer, count: Int) -> Int {
      lock.lock()
      defer { lock.unlock() }
      guard (try? cancellation.check()) != nil else { return 0 }
      guard count <= limit - data.count else {
        exceeded = true
        return 0
      }
      data.append(pointer.assumingMemoryBound(to: UInt8.self), count: count)
      return count
    }
    func result(success: Bool) throws -> Data {
      lock.lock()
      defer { lock.unlock() }
      try cancellation.check()
      guard !exceeded else { throw ShareFailure.quota }
      guard success, !data.isEmpty else { throw ShareFailure.invalid }
      return data
    }
  }
  private static func encode(
    _ image: CGImage, type: UTType, limit: Int,
    cancellation: ShareCancellation
  ) throws -> Data {
    let sink = Sink(limit: limit, cancellation: cancellation)
    var callbacks = CGDataConsumerCallbacks(
      putBytes: { info, bytes, count in
        guard let info else { return 0 }
        return Unmanaged<Sink>.fromOpaque(info).takeUnretainedValue().append(bytes, count: count)
      }, releaseConsumer: nil)
    guard
      let consumer = CGDataConsumer(
        info: Unmanaged.passUnretained(sink).toOpaque(), cbks: &callbacks),
      let destination = CGImageDestinationCreateWithDataConsumer(
        consumer, type.identifier as CFString, 1, nil)
    else { throw ShareFailure.invalid }
    return try withExtendedLifetime(sink) {
      CGImageDestinationAddImage(
        destination, image,
        [kCGImageDestinationLossyCompressionQuality: 0.82] as CFDictionary)
      return try sink.result(success: CGImageDestinationFinalize(destination))
    }
  }

  /// A display-only JPEG. Original bytes never cross the WebView bridge.
  public static func thumbnail(_ url: URL) throws -> Data {
    try encode(
      image(source(url), pixels: 480), type: .jpeg,
      limit: ShareLimits.thumbnailBytes, cancellation: ShareCancellation())
  }

  /// Nil means send the original without conversion or upscaling.
  /// PNG stays lossless with alpha; reduced JPEG/HEIC/HEIF becomes JPEG.
  public static func resized(
    _ url: URL, size: SharePhotoSize,
    cancellation: ShareCancellation = ShareCancellation()
  ) throws -> Output? {
    try cancellation.check()
    guard let pixels = size.pixels else { return nil }
    let source = try source(url)
    let mime = try detectedMime(source)
    guard ["image/jpeg", "image/png", "image/heic", "image/heif"].contains(mime),
      CGImageSourceGetCount(source) == 1
    else { throw ShareFailure.photoResizeUnsupported }
    let (width, height) = try dimensions(source)
    guard max(width, height) > pixels else { return nil }
    let image = try image(source, pixels: pixels)
    try cancellation.check()
    let png = mime == "image/png"
    let data = try encode(
      image, type: png ? .png : .jpeg,
      limit: ShareLimits.resizedPhotoBytes, cancellation: cancellation)
    return Output(
      data: data, mime: png ? "image/png" : "image/jpeg", fileExtension: png ? "png" : "jpg")
  }
}
