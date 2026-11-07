import Foundation

public enum ShareFailure: String, Error, Codable {
  case unavailable, invalid, quota, stale, busy, missingFile, expired, authentication, invalidation,
    httpsRequired, cancelled, previewUnavailable, invalidResponse, pendingLimit, previewBusy,
    transferBusy, sending, publicIPv4Required, photoResizeUnsupported
}

public enum ShareLimits {
  public static let files = 10
  public static let batchBytes = 100 * 1024 * 1024
  public static let pendingBytes = 200 * 1024 * 1024
  public static let pendingCount = 20
  public static let textBytes = 64 * 1024
  // Escaping a 64 KiB text import can expand its JSON representation sixfold.
  public static let manifestBytes = 1024 * 1024
  public static let metadataReservation = 2 * manifestBytes
  public static let resizedPhotoBytes = 20 * 1024 * 1024
  public static let thumbnailBytes = 2 * 1024 * 1024
  // Room lists include topics and other fields the native picker does not use.
  public static let roomListBytes = 8 * 1024 * 1024
  public static let responseBytes = max(roomListBytes, thumbnailBytes)
  public static let retention: TimeInterval = 24 * 60 * 60
  public static let lease: TimeInterval = 300
}

public enum DeliveryState: String, Codable {
  case ready, uploading, uploaded, dispatching, sent, failed, unknown
}

public struct ShareOperation: Codable, Equatable {
  public var id: String
  public var state: DeliveryState = .ready
  public var uploadId: String?
  public var eventId: String?
  public var failure: String?
  public init(id: String = UUID().uuidString) { self.id = id }
}

public enum SharePhotoSize: String, Codable, CaseIterable, Hashable {
  case original, large, medium, small
  public var pixels: Int? {
    switch self {
    case .original: return nil
    case .large: return 2048
    case .medium: return 1280
    case .small: return 640
    }
  }
  public var title: String { rawValue.capitalized }
  public var label: String { pixels.map { "\(title) (\($0) px)" } ?? title }
}

public struct ShareFile: Codable, Equatable {
  public var id: String
  public var name: String
  public var mime: String
  public var bytes: Int
  public var digest: String
  public var thumbnail: Bool
  public var operation: ShareOperation?
  // Originals stay immutable. Recovery regenerates this chosen representation.
  public var photoSize: SharePhotoSize?
  public var photoBytes: Int?
  // Captions belong uniformly to photo IDs; source text is a separate message.
  public var caption: String?
  public var uploadBytes: Int { photoBytes ?? bytes }
}

public struct SharePreview: Codable, Equatable {
  public var matched_url: String
  public var title: String?
  public var description: String?
  public var site_name: String?
  public var image_upload_id: String?
  public var provenance: String
  public var thumbnail: String?
  public init(
    matched_url: String, title: String? = nil, description: String? = nil,
    site_name: String? = nil, image_upload_id: String? = nil,
    provenance: String = "source", thumbnail: String? = nil
  ) {
    self.matched_url = matched_url
    self.title = title
    self.description = description
    self.site_name = site_name
    self.image_upload_id = image_upload_id
    self.provenance = provenance
    self.thumbnail = thumbnail
  }
}

public struct ShareManifest: Codable, Equatable {
  public var version = 4
  public var id: String
  public var createdAt: TimeInterval
  public var session: String
  public var accountId: String?
  public var roomId: String?
  public var text: String
  public var previews: [SharePreview]?
  public var files: [ShareFile] = []
  public var operation = ShareOperation()
  public var importFailures: [String] = []
  public var owner: String?
  public var leaseUntil: TimeInterval?
  public var revision = 0
  public init(
    id: String = UUID().uuidString, createdAt: TimeInterval,
    session: String, text: String
  ) {
    self.id = id
    self.createdAt = createdAt
    self.session = session
    self.text = text
  }
  /// Compatibility is confined to this boundary; live consumers use v4 fields.
  public func canonicalized() -> ShareManifest {
    guard version < 4 else { return self }
    var next = self
    if let first = files.first(where: { !$0.thumbnail }), !text.isEmpty,
      let delivery = first.operation, [.sent, .dispatching, .unknown].contains(delivery.state) {
      next.operation.state = delivery.state == .dispatching ? .unknown : delivery.state
      next.operation.eventId = delivery.eventId
      next.operation.failure = delivery.failure
    }
    for index in next.files.indices where !next.files[index].thumbnail {
      next.files[index].caption = next.files[index].caption ?? ""
    }
    next.version = 4
    return next
  }
  public var hasMessage: Bool { !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
  public var deliveryOperations: [ShareOperation] {
    let photos = files.compactMap(\.operation)
    return hasMessage || photos.isEmpty ? [operation] + photos : photos
  }
  public func caption(for file: ShareFile) -> String { file.caption ?? "" }
  public var captionBytes: Int {
    files.reduce(text.utf8.count) { $0 + ($1.caption?.utf8.count ?? 0) }
  }
  public var bytes: Int { files.reduce(captionBytes) { $0 + $1.bytes } }
  public var uploadBytes: Int { files.reduce(captionBytes) { $0 + $1.uploadBytes } }
  public var completed: Bool { deliveryOperations.allSatisfy { $0.state == .sent } }
}

/// No credentials or caller-provided endpoint URLs occur in a manifest.
public struct ShareSession: Codable, Equatable {
  public var installation: String
  public var generation: String
  public var revision: String
  public var server: String
  public var credentialAvailable: Bool
  // Optional for older descriptors. A remembered access credential was the
  // only unambiguous staging grant before this field was introduced.
  public var stagingAllowed: Bool?
  public var expiresAt: TimeInterval?
  public init(
    installation: String, generation: String = UUID().uuidString,
    revision: String = UUID().uuidString, server: String,
    credentialAvailable: Bool, expiresAt: TimeInterval?, stagingAllowed: Bool? = nil
  ) {
    self.installation = installation
    self.generation = generation
    self.revision = revision
    self.server = server
    self.credentialAvailable = credentialAvailable
    self.stagingAllowed = stagingAllowed
    self.expiresAt = expiresAt
  }
}

public enum ShareJSON {
  public static func encode<T: Encodable>(_ value: T) throws -> Data {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    return try encoder.encode(value)
  }
}
