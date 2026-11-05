import Foundation
import ImageIO
import UniformTypeIdentifiers

/// Inspect protected copies, never trust a provider's declared MIME or suffix.
/// Original bytes (including HEIC) stay intact; only display thumbnails decode.
public enum PhotoShare {
  private static func source(_ url: URL) throws -> CGImageSource {
    guard
      let source = CGImageSourceCreateWithURL(
        url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
      CGImageSourceGetCount(source) > 0,
      let properties = CGImageSourceCopyPropertiesAtIndex(
        source, CGImageSourceGetPrimaryImageIndex(source), nil) as? [CFString: Any],
      let width = properties[kCGImagePropertyPixelWidth] as? Int,
      let height = properties[kCGImagePropertyPixelHeight] as? Int,
      width > 0, height > 0, width <= 65535, height <= 65535,
      width <= 200_000_000 / height
    else { throw ShareFailure.invalid }
    return source
  }

  public static func mime(_ url: URL) throws -> String {
    let source = try source(url)
    guard let identifier = CGImageSourceGetType(source),
      let type = UTType(identifier as String), type.conforms(to: .image),
      let mime = type.preferredMIMEType, mime.hasPrefix("image/"),
      mime != "image/svg+xml"
    else { throw ShareFailure.invalid }
    _ = try image(source)
    return mime
  }

  private static func image(_ source: CGImageSource) throws -> CGImage {
    guard
      let image = CGImageSourceCreateThumbnailAtIndex(
        source, CGImageSourceGetPrimaryImageIndex(source),
        [
          kCGImageSourceCreateThumbnailFromImageAlways: true,
          kCGImageSourceThumbnailMaxPixelSize: 480,
          kCGImageSourceCreateThumbnailWithTransform: true,
          kCGImageSourceShouldCacheImmediately: true,
        ] as CFDictionary)
    else { throw ShareFailure.invalid }
    return image
  }

  /// A bounded JPEG for both native and WebView recovery. Never return the
  /// original file across the bridge or allocate a full-resolution UIImage.
  public static func thumbnail(_ url: URL) throws -> Data {
    let source = try source(url)
    let image = try image(source)
    let data = NSMutableData()
    guard
      let destination = CGImageDestinationCreateWithData(
        data, UTType.jpeg.identifier as CFString, 1, nil)
    else { throw ShareFailure.invalid }
    CGImageDestinationAddImage(
      destination, image, [kCGImageDestinationLossyCompressionQuality: 0.75] as CFDictionary)
    guard CGImageDestinationFinalize(destination), data.length <= ShareLimits.thumbnailBytes else {
      throw ShareFailure.quota
    }
    return data as Data
  }
}
