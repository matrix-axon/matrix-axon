#if os(iOS)
  import Foundation
  import ImageIO
  import LinkPresentation
  import UniformTypeIdentifiers
  import os

  public enum DevicePreviewFailure: String, Error {
    case unavailable, timeout, noMetadata, unsupportedURL

    public var explanation: String {
      switch self {
      case .unavailable: return "The on-device preview lookup is unavailable."
      case .timeout: return "The on-device preview lookup timed out."
      case .noMetadata: return "The device found no usable preview metadata for this link."
      case .unsupportedURL: return "This address is not supported for on-device previews."
      }
    }
  }

  /// One explicit lookup on the composer's serial worker. It has no credential
  /// or homeserver transport dependency. Apple owns the metadata network work;
  /// this adapter bounds its lifetime and all bytes retained in our draft.
  public final class DevicePreview {
    private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "device-preview")
    public typealias Lookup = (URL, ShareCancellation) throws -> LPLinkMetadata
    public struct Outcome {
      public let manifest: ShareManifest
      public let image: SharePreviewImageState
      public let reused: Bool
    }
    private let store: ShareStore
    private let cancellation: ShareCancellation
    private let lookup: Lookup?

    public init(
      store: ShareStore, cancellation: ShareCancellation = ShareCancellation(),
      lookup: Lookup? = nil
    ) {
      self.store = store
      self.cancellation = cancellation
      self.lookup = lookup
    }

    static func permits(_ raw: String) -> Bool {
      guard let url = URL(string: raw) else { return false }
      return (try? PreviewNetwork.validate(url)) != nil
    }

    public func include(_ input: ShareManifest, url: String, refresh: Bool = false) throws
      -> Outcome
    {
      try cancellation.check()
      guard TextShare.links(input.text).contains(url) else {
        throw DevicePreviewFailure.unsupportedURL
      }
      guard
        (input.previews?.count ?? 0) < 8
          || input.previews?.contains(where: { $0.matched_url == url }) == true
      else { throw ShareFailure.invalid }
      try validateOwner(input)
      let previous = input.previews?.first(where: { $0.matched_url == url })
      if let previous, !refresh {
        var image = SharePreviewImageState.absent
        if let file = previous.thumbnail {
          try store.withVerifiedFile(input.id, file: file, owner: input.owner!) { _, _ in }
          image = .ready
        } else if previous.image_upload_id != nil {
          image = .unavailable
        }
        Self.log.info("device_preview_reused image=\(image.rawValue, privacy: .public)")
        return Outcome(manifest: input, image: image, reused: true)
      }
      guard Self.permits(url), let address = URL(string: url) else {
        throw DevicePreviewFailure.unsupportedURL
      }
      let metadata: RestrictedPreview.Metadata
      var suppliedImage: NSItemProvider?
      do {
        if let lookup {
          let supplied = try lookup(address, cancellation)
          suppliedImage = supplied.imageProvider
          metadata = RestrictedPreview.Metadata(
            originalURL: supplied.originalURL ?? address,
            url: supplied.url ?? address, title: supplied.title)
        } else {
          metadata = try RestrictedPreview.fetch(address, cancellation: cancellation)
        }
      } catch {
        try cancellation.check()
        if let failure = error as? DevicePreviewFailure { throw failure }
        if (error as? ShareFailure) == .expired { throw DevicePreviewFailure.timeout }
        throw DevicePreviewFailure.unavailable
      }
      try cancellation.check()
      let original = metadata.originalURL
      do {
        guard
          PreviewContract.canonicalURL(original.absoluteString) == PreviewContract.canonicalURL(url)
        else { throw DevicePreviewFailure.noMetadata }
      }
      if !Self.permits(metadata.url.absoluteString) {
        throw DevicePreviewFailure.unsupportedURL
      }
      // Metadata is parsed as inert HTML; existing source fields remain fallback.
      let title =
        TextShare.previewField(metadata.title, limit: 300)
        ?? TextShare.previewField(previous?.title, limit: 300)
      var preview = SharePreview(
        matched_url: url, title: title,
        description: TextShare.previewField(metadata.description, limit: 1000)
          ?? TextShare.previewField(previous?.description, limit: 1000),
        site_name: TextShare.previewField(metadata.siteName, limit: 120)
          ?? TextShare.previewField(previous?.site_name, limit: 120), provenance: "generated")
      var image: SharePreviewImageState = metadata.imageUnavailable ? .unavailable : .absent
      var retained: (Data, String)?
      if let data = metadata.image {
        image = .unavailable
        retained = try? Self.normalizeImage(data)
      } else if let provider = suppliedImage {
        image = .unavailable
        do { retained = try Self.image(provider, cancellation: cancellation) } catch {
          try cancellation.check()
          Self.log.info("device_preview_image_unavailable")
        }
      }
      try cancellation.check()
      try validateOwner(input)
      if let (data, mime) = retained {
        do {
          let saved = try store.importThumbnail(
            input.id, owner: input.owner!, data: data, mime: mime, preview: preview)
          Self.log.info("device_preview_included image=ready")
          return Outcome(manifest: saved, image: .ready, reused: false)
        } catch {
          guard (error as? ShareFailure) == .quota else { throw error }
          image = .unavailable
        }
      }
      guard preview.title != nil || preview.description != nil || preview.site_name != nil else {
        throw DevicePreviewFailure.noMetadata
      }
      // A refresh is best effort. Only explicit Remove image may throw away a
      // good retained image when the replacement has no usable image bytes.
      if let file = previous?.thumbnail,
        (try? store.withVerifiedFile(input.id, file: file, owner: input.owner!) { _, _ in true })
          == true
      {
        preview.thumbnail = file
        preview.image_upload_id = previous?.image_upload_id
        image = .ready
      }
      var next = input
      next.previews = (next.previews ?? []).filter { $0.matched_url != url } + [preview]
      next = try store.update(next)
      Self.log.info("device_preview_included image=\(image.rawValue, privacy: .public)")
      return Outcome(manifest: next, image: image, reused: false)
    }

    private func validateOwner(_ input: ShareManifest) throws {
      let saved = try store.owned(input.id, owner: input.owner ?? "", account: input.accountId)
      guard saved.session == input.session, saved.revision == input.revision else {
        throw ShareFailure.stale
      }
    }

    private static func image(
      _ provider: NSItemProvider, cancellation: ShareCancellation
    ) throws -> (Data, String) {
      let types = [UTType.jpeg, .png, .webP, .image].filter {
        provider.hasItemConformingToTypeIdentifier($0.identifier)
      }
      let deadline = Date().addingTimeInterval(10)
      for type in types {
        try cancellation.check()
        guard Date() < deadline else { break }
        do {
          let data = try ShareIntake.bytes(
            provider, type: type.identifier, limit: ShareLimits.thumbnailBytes,
            timeout: deadline.timeIntervalSinceNow, cancellation: cancellation)
          let normalized = try normalizeImage(data)
          log.info("device_preview_image retained_bytes=\(normalized.0.count)")
          return normalized
        } catch {
          try cancellation.check()
          log.info(
            "device_preview_image_representation_failed category=\((error as? ShareFailure)?.rawValue ?? "unavailable", privacy: .public)"
          )
        }
      }
      throw ShareFailure.unavailable
    }

    static func normalizeImage(_ data: Data) throws -> (Data, String) {
      let source = try ShareStore.checkedImageSource(data)
      let type = CGImageSourceGetType(source) as String?
      for format in [UTType.png, .jpeg, .webP] where type == format.identifier {
        return (data, format.preferredMIMEType!)
      }
      // Generic public image providers can vend TIFF/HEIC. Convert frame zero
      // only, after the shared byte/pixel checks, into the staging contract.
      guard
        let image = CGImageSourceCreateThumbnailAtIndex(
          source, 0,
          [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceThumbnailMaxPixelSize: 1200,
            kCGImageSourceCreateThumbnailWithTransform: true,
          ] as CFDictionary)
      else { throw ShareFailure.invalid }
      let output = NSMutableData()
      guard
        let destination = CGImageDestinationCreateWithData(
          output, UTType.jpeg.identifier as CFString, 1, nil)
      else { throw ShareFailure.invalid }
      CGImageDestinationAddImage(
        destination, image, [kCGImageDestinationLossyCompressionQuality: 0.8] as CFDictionary)
      guard CGImageDestinationFinalize(destination), output.length <= ShareLimits.thumbnailBytes
      else {
        throw ShareFailure.quota
      }
      return (output as Data, "image/jpeg")
    }
  }
#endif
