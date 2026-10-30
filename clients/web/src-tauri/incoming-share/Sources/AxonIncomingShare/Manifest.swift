import Foundation

public enum ShareFailure: String, Error, Codable {
  case unavailable, invalid, quota, stale, busy, missingFile, expired, authentication, invalidation,
    httpsRequired
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
  public static let thumbnailBytes = 2 * 1024 * 1024
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

public struct ShareFile: Codable, Equatable {
  public var id: String
  public var name: String
  public var mime: String
  public var bytes: Int
  public var digest: String
  public var thumbnail: Bool
  public var operation: ShareOperation?
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
  public var version = 1
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
  public var bytes: Int { files.reduce(text.utf8.count) { $0 + $1.bytes } }
  public var completed: Bool {
    let operations = files.compactMap(\.operation)
    return operations.isEmpty
      ? operation.state == .sent : operations.allSatisfy { $0.state == .sent }
  }
}

/// No credentials or caller-provided endpoint URLs occur in a manifest.
public struct ShareSession: Codable, Equatable {
  public var installation: String
  public var generation: String
  public var revision: String
  public var server: String
  public var credentialAvailable: Bool
  public var expiresAt: TimeInterval?
  public init(
    installation: String, generation: String = UUID().uuidString,
    revision: String = UUID().uuidString, server: String,
    credentialAvailable: Bool, expiresAt: TimeInterval?
  ) {
    self.installation = installation
    self.generation = generation
    self.revision = revision
    self.server = server
    self.credentialAvailable = credentialAvailable
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
