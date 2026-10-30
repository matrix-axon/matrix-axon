import CryptoKit
import Darwin
import Foundation
import ImageIO
import os

/// One lock guards the descriptor, manifests, quota reservations and ownership.
/// Provider and network operations run outside it. Every completion carries a
/// session generation and owner fence; a lease expiry never authorizes replay.
public final class ShareStore {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "incoming-share")
  public let root: URL
  private let now: () -> TimeInterval
  private let fm = FileManager.default

  public init(root: URL, now: @escaping () -> TimeInterval = { Date().timeIntervalSince1970 })
    throws
  {
    self.root = root
    self.now = now
    try fm.createDirectory(at: root, withIntermediateDirectories: true)
    guard (try root.resourceValues(forKeys: [.isSymbolicLinkKey])).isSymbolicLink != true else {
      throw ShareFailure.invalid
    }
    try protect(root)
  }

  public func coordinated<T>(_ action: () throws -> T) throws -> T {
    let descriptor = open(
      root.appendingPathComponent(".lock").path, O_CREAT | O_RDWR | O_NOFOLLOW | O_NONBLOCK, 0o600)
    guard descriptor >= 0 else { throw ShareFailure.unavailable }
    defer { close(descriptor) }
    var lockInfo = stat()
    guard fstat(descriptor, &lockInfo) == 0, (lockInfo.st_mode & S_IFMT) == S_IFREG else {
      throw ShareFailure.invalid
    }
    let deadline = Date().addingTimeInterval(3)
    while flock(descriptor, LOCK_EX | LOCK_NB) != 0 {
      guard errno == EWOULDBLOCK, Date() < deadline else { throw ShareFailure.busy }
      usleep(10_000)
    }
    defer { flock(descriptor, LOCK_UN) }
    return try action()
  }

  public func session() throws -> ShareSession? {
    try coordinated { try readSession() }
  }

  /// The app alone calls this. Replacing identity invalidates content before
  /// publishing a descriptor for the replacement; refresh preserves generation.
  public func publish(_ session: ShareSession, credential: () throws -> Void) throws {
    _ = try publish(make: { _ in session }, credential: credential)
  }

  public func publish(
    make: (ShareSession?) throws -> ShareSession,
    credential: () throws -> Void
  ) throws -> ShareSession {
    try coordinated {
      let intent = root.appendingPathComponent("session.pending.json")
      let previous = try readStagingSession()
      let session = try make(previous)
      // Remove authority first. A killed Keychain writer leaves no readable
      // descriptor, never a new descriptor paired with an old credential.
      let descriptor = root.appendingPathComponent("session.json")
      try write(session, to: intent)
      if fm.fileExists(atPath: descriptor.path) { try fm.removeItem(at: descriptor) }
      let replaced =
        previous?.generation != session.generation || previous?.installation != session.installation
      for item in try directories() {
        let old = try? load(item.lastPathComponent)
        if replaced || (old != nil && old?.session != session.generation) {
          try fm.removeItem(at: item)
        }
      }
      try credential()
      try write(session, to: descriptor)
      try fm.removeItem(at: intent)
      return session
    }
  }

  // Only for operations already holding the store's lock.
  func sessionLocked() throws -> ShareSession? { try readSession() }

  public func create(text: String) throws -> ShareManifest {
    try coordinated {
      guard let session = try readStagingSession() else { throw ShareFailure.unavailable }
      guard text.utf8.count <= ShareLimits.textBytes else { throw ShareFailure.quota }
      let records = try reconcileLocked()
      guard records.count < ShareLimits.pendingCount,
        (try usedBytes(records)) + text.utf8.count + ShareLimits.metadataReservation
          <= ShareLimits.pendingBytes
      else { throw ShareFailure.quota }
      let manifest = ShareManifest(createdAt: now(), session: session.generation, text: text)
      let directory = try directory(manifest.id)
      try fm.createDirectory(at: directory, withIntermediateDirectories: false)
      try protect(directory)
      try save(manifest)
      Self.log.info(
        "share_created share_id=\(manifest.id, privacy: .public) bytes=\(manifest.bytes)")
      return manifest
    }
  }

  public func list() throws -> [ShareManifest] { try coordinated { try reconcileLocked() } }

  public func clearAccount(_ account: String) throws {
    guard UUID(uuidString: account) != nil else { throw ShareFailure.invalid }
    try coordinated {
      for manifest in try readAll() where manifest.accountId == account {
        try fm.removeItem(at: directory(manifest.id))
      }
    }
  }

  public func claim(_ id: String) throws -> ShareManifest {
    try coordinated {
      _ = try reconcileLocked()
      var manifest = try load(id)
      try validateSession(manifest)
      guard manifest.owner == nil else { throw ShareFailure.busy }
      manifest.owner = UUID().uuidString
      manifest.leaseUntil = min(
        now() + ShareLimits.lease, manifest.createdAt + ShareLimits.retention)
      manifest.revision += 1
      try save(manifest)
      return manifest
    }
  }

  public func update(_ proposed: ShareManifest, release: Bool = false) throws -> ShareManifest {
    try coordinated {
      let current = try load(proposed.id)
      try validateSession(current)
      guard proposed.session == current.session, proposed.owner != nil,
        proposed.owner == current.owner, proposed.revision == current.revision,
        (current.leaseUntil ?? 0) > now(), proposed.files.count == current.files.count,
        proposed.createdAt == current.createdAt, proposed.version == 1
      else { throw ShareFailure.stale }
      for (before, after) in zip(current.files, proposed.files) {
        var descriptor = after
        descriptor.operation = before.operation
        guard before == descriptor else { throw ShareFailure.invalid }
        if let from = before.operation, let to = after.operation {
          try validateTransition(from: from, to: to)
        } else if before.operation != after.operation {
          throw ShareFailure.invalid
        }
      }
      guard proposed.text.utf8.count <= ShareLimits.textBytes,
        (proposed.previews?.count ?? 0) <= 8
      else { throw ShareFailure.quota }
      guard
        proposed.previews?.allSatisfy({ ["source", "generated"].contains($0.provenance) }) ?? true
      else { throw ShareFailure.invalid }
      guard
        proposed.bytes + (try reservations(in: directory(proposed.id))) <= ShareLimits.batchBytes,
        (try usedBytes(readAll())) - current.bytes + proposed.bytes <= ShareLimits.pendingBytes
      else { throw ShareFailure.quota }
      // Confirmed results are immutable, including their destination/caption.
      let fixedDestination = current.files.contains { file in
        guard let state = file.operation?.state else { return false }
        return state == .sent || state == .dispatching || state == .unknown
      }
      if current.operation.state == .sent || current.operation.state == .dispatching
        || current.operation.state == .unknown || fixedDestination
      {
        guard current.operation.state != .sent || proposed.operation == current.operation,
          proposed.text == current.text,
          proposed.accountId == current.accountId, proposed.roomId == current.roomId,
          proposed.previews == current.previews
        else { throw ShareFailure.invalid }
      }
      try validateTransition(from: current.operation, to: proposed.operation)
      var next = proposed
      let retainedThumbnails = Set(next.previews?.compactMap(\.thumbnail) ?? [])
      let removed = current.files.filter { file in
        file.thumbnail && current.previews?.contains(where: { $0.thumbnail == file.id }) == true
          && !retainedThumbnails.contains(file.id)
      }
      next.files.removeAll { file in removed.contains(where: { $0.id == file.id }) }
      next.revision += 1
      next.leaseUntil =
        release ? nil : min(now() + ShareLimits.lease, next.createdAt + ShareLimits.retention)
      if release { next.owner = nil }
      try save(next)
      Self.log.info(
        "share_updated share_id=\(next.id, privacy: .public) state=\(next.operation.state.rawValue, privacy: .public) revision=\(next.revision)"
      )
      for file in next.files {
        if let operation = file.operation,
          current.files.first(where: { $0.id == file.id })?.operation != operation
        {
          Self.log.info(
            "share_item_updated share_id=\(next.id, privacy: .public) item_id=\(file.id, privacy: .public) state=\(operation.state.rawValue, privacy: .public)"
          )
        }
      }
      for file in removed {
        try? fm.removeItem(at: directory(next.id).appendingPathComponent(file.id))
      }
      return next
    }
  }

  public func discard(_ id: String, owner: String? = nil, completed: Bool = false) throws {
    try coordinated {
      let current: ShareManifest
      do { current = try load(id) } catch is DecodingError {
        guard !completed else { throw ShareFailure.invalid }
        try fm.removeItem(at: directory(id))
        return
      } catch ShareFailure.invalid {
        guard !completed else { throw ShareFailure.invalid }
        try fm.removeItem(at: directory(id))
        return
      }
      try validateSession(current)
      guard current.owner == nil || current.owner == owner || (current.leaseUntil ?? 0) <= now()
      else { throw ShareFailure.busy }
      if completed && !current.completed { throw ShareFailure.invalid }
      try fm.removeItem(at: directory(id))
    }
  }

  /// Copies temporary provider files while their access grant is still valid.
  /// Reserve the maximum accepted size first; a killed writer's reservation is
  /// kept until its fenced lease expires. No full-file allocation is needed.
  public func importFile(
    _ id: String, owner: String, source: URL, name: String,
    mime: String, thumbnail: Bool = false
  ) throws -> ShareManifest {
    guard source.isFileURL else { throw ShareFailure.invalid }
    guard mime.utf8.count <= 128, mime.rangeOfCharacter(from: .controlCharacters) == nil else {
      throw ShareFailure.invalid
    }
    let scoped = source.startAccessingSecurityScopedResource()
    defer { if scoped { source.stopAccessingSecurityScopedResource() } }
    let fileId = UUID().uuidString
    var cap = thumbnail ? ShareLimits.thumbnailBytes : ShareLimits.batchBytes
    let reservation = try coordinated { () throws -> URL in
      let manifest = try load(id)
      try validateOwner(manifest, owner)
      guard thumbnail || manifest.files.filter({ !$0.thumbnail }).count < ShareLimits.files
      else { throw ShareFailure.quota }
      guard !thumbnail || manifest.files.filter(\.thumbnail).count < 16 else {
        throw ShareFailure.quota
      }
      let records = try readAll()
      let pending = try usedBytes(records)
      let local = try reservations(in: directory(id))
      cap = min(
        cap, ShareLimits.batchBytes - manifest.bytes - local,
        ShareLimits.pendingBytes - pending)
      guard cap > 0 else { throw ShareFailure.quota }
      let target = try directory(id).appendingPathComponent(fileId + ".part")
      try write(cap, to: target.appendingPathExtension("reservation"))
      return target
    }
    defer {
      try? coordinated {
        try? fm.removeItem(at: reservation)
        try? fm.removeItem(at: reservation.appendingPathExtension("reservation"))
      }
    }
    let input = open(source.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
    guard input >= 0 else { throw ShareFailure.missingFile }
    defer { close(input) }
    var info = stat()
    guard fstat(input, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG else {
      throw ShareFailure.invalid
    }
    guard info.st_size <= cap else { throw ShareFailure.quota }
    let output = open(reservation.path, O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW, 0o600)
    guard output >= 0 else { throw ShareFailure.unavailable }
    defer { close(output) }
    try protect(reservation)
    var buffer = [UInt8](repeating: 0, count: 64 * 1024)
    var bytes = 0
    var hash = SHA256()
    let deadline = Date().addingTimeInterval(60)
    while true {
      guard Date() < deadline else { throw ShareFailure.expired }
      let count = Darwin.read(input, &buffer, buffer.count)
      guard count >= 0 else { throw ShareFailure.unavailable }
      if count == 0 { break }
      bytes += count
      guard bytes <= cap else { throw ShareFailure.quota }
      hash.update(data: Data(buffer.prefix(count)))
      try buffer.withUnsafeBytes { raw in
        var offset = 0
        while offset < count {
          let written = Darwin.write(output, raw.baseAddress!.advanced(by: offset), count - offset)
          guard written > 0 else { throw ShareFailure.unavailable }
          offset += written
        }
      }
    }
    guard fsync(output) == 0 else { throw ShareFailure.unavailable }
    if thumbnail { try Self.validateThumbnail(Data(contentsOf: reservation), mime: mime) }
    return try coordinated {
      var manifest = try load(id)
      try validateOwner(manifest, owner)
      guard thumbnail || manifest.files.filter({ !$0.thumbnail }).count < ShareLimits.files else {
        throw ShareFailure.quota
      }
      guard manifest.bytes + bytes <= ShareLimits.batchBytes else { throw ShareFailure.quota }
      let destination = try directory(id).appendingPathComponent(fileId)
      try fm.moveItem(at: reservation, to: destination)
      manifest.files.append(
        ShareFile(
          id: fileId, name: Self.displayName(name), mime: mime,
          bytes: bytes, digest: hash.finalize().map { String(format: "%02x", $0) }.joined(),
          thumbnail: thumbnail, operation: thumbnail ? nil : ShareOperation()))
      manifest.revision += 1
      try save(manifest)
      return manifest
    }
  }

  private static func displayName(_ input: String) -> String {
    var bounded = ""
    var bytes = 0
    for scalar in input.unicodeScalars {
      let count = scalar.utf8.count
      if bytes + count > 255 { break }
      bytes += count
      if !CharacterSet.controlCharacters.contains(scalar)
        && scalar.properties.generalCategory != .format
      {
        bounded.unicodeScalars.append(scalar)
      }
    }
    let leaf =
      bounded.replacingOccurrences(of: "\\", with: "/").split(separator: "/").last.map(String.init)
      ?? ""
    let name = leaf.trimmingCharacters(in: .whitespacesAndNewlines)
    return name.isEmpty || name == "." || name == ".." ? "attachment" : name
  }

  public func readFile(_ id: String, file: String, owner: String, offset: Int, count: Int) throws
    -> Data
  {
    try coordinated {
      let manifest = try load(id)
      try validateOwner(manifest, owner)
      guard let item = manifest.files.first(where: { $0.id == file }), offset >= 0,
        count > 0, count <= 64 * 1024, offset <= item.bytes
      else { throw ShareFailure.invalid }
      let url = try directory(id).appendingPathComponent(item.id)
      let descriptor = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
      guard descriptor >= 0 else { throw ShareFailure.missingFile }
      defer { close(descriptor) }
      var data = Data(count: min(count, item.bytes - offset))
      let expected = data.count
      let actual = data.withUnsafeMutableBytes {
        pread(descriptor, $0.baseAddress, expected, off_t(offset))
      }
      guard actual == expected else { throw ShareFailure.missingFile }
      return data
    }
  }

  public func importThumbnail(_ id: String, owner: String, data: Data, mime: String) throws
    -> ShareManifest
  {
    guard data.count <= ShareLimits.thumbnailBytes else { throw ShareFailure.quota }
    try Self.validateThumbnail(data, mime: mime)
    return try coordinated {
      var manifest = try load(id)
      try validateOwner(manifest, owner)
      guard manifest.files.filter(\.thumbnail).count < 16 else { throw ShareFailure.quota }
      guard
        manifest.bytes + data.count + (try reservations(in: directory(id)))
          <= ShareLimits.batchBytes,
        (try usedBytes(readAll())) + data.count <= ShareLimits.pendingBytes
      else { throw ShareFailure.quota }
      let fileId = UUID().uuidString
      try writeBytes(data, to: directory(id).appendingPathComponent(fileId))
      manifest.files.append(
        ShareFile(
          id: fileId, name: "preview", mime: mime, bytes: data.count,
          digest: SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined(),
          thumbnail: true))
      manifest.revision += 1
      try save(manifest)
      return manifest
    }
  }

  private static func validateThumbnail(_ data: Data, mime: String) throws {
    let detected: String?
    if data.starts(with: [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
      detected = "image/png"
    } else if data.starts(with: [0xff, 0xd8, 0xff]) {
      detected = "image/jpeg"
    } else if data.count >= 12 && data.prefix(4) == Data("RIFF".utf8)
      && data[8..<12] == Data("WEBP".utf8)
    {
      detected = "image/webp"
    } else {
      detected = nil
    }
    guard detected == mime,
      let source = CGImageSourceCreateWithData(
        data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
      let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
      let width = properties[kCGImagePropertyPixelWidth] as? Int,
      let height = properties[kCGImagePropertyPixelHeight] as? Int,
      width > 0, height > 0, width <= 4_000_000, height <= 4_000_000,
      width <= 4_000_000 / height
    else { throw ShareFailure.invalid }
  }

  /// Verify retained bytes before an upload. Missing/corrupt data is visible
  /// recovery work, never a reason to regenerate the linked page implicitly.
  public func verifiedFile(_ id: String, file: String, owner: String) throws -> (URL, ShareFile) {
    try coordinated {
      let manifest = try load(id)
      try validateOwner(manifest, owner)
      guard let item = manifest.files.first(where: { $0.id == file }) else {
        throw ShareFailure.missingFile
      }
      let url = try directory(id).appendingPathComponent(item.id)
      let descriptor = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
      guard descriptor >= 0 else { throw ShareFailure.missingFile }
      defer { close(descriptor) }
      var info = stat()
      guard fstat(descriptor, &info) == 0, info.st_size == item.bytes else {
        throw ShareFailure.missingFile
      }
      var hash = SHA256()
      var buffer = [UInt8](repeating: 0, count: 64 * 1024)
      while true {
        let count = Darwin.read(descriptor, &buffer, buffer.count)
        guard count >= 0 else { throw ShareFailure.missingFile }
        if count == 0 { break }
        hash.update(data: Data(buffer.prefix(count)))
      }
      guard hash.finalize().map({ String(format: "%02x", $0) }).joined() == item.digest else {
        throw ShareFailure.missingFile
      }
      return (url, item)
    }
  }

  public func validateSession(_ manifest: ShareManifest) throws {
    guard let session = try readStagingSession(), session.generation == manifest.session else {
      throw ShareFailure.stale
    }
  }

  private func validateOwner(_ manifest: ShareManifest, _ owner: String) throws {
    try validateSession(manifest)
    guard manifest.owner == owner, (manifest.leaseUntil ?? 0) > now() else {
      throw ShareFailure.stale
    }
  }

  private func validateTransition(from: ShareOperation, to: ShareOperation) throws {
    guard from.id == to.id else { throw ShareFailure.invalid }
    if from == to { return }
    let allowed: [DeliveryState: Set<DeliveryState>] = [
      .ready: [.uploading, .dispatching, .failed], .uploading: [.uploaded, .failed],
      .uploaded: [.uploading, .dispatching, .failed], .dispatching: [.sent, .failed, .unknown],
      .failed: [.ready], .unknown: [.ready], .sent: [],
    ]
    guard allowed[from.state]?.contains(to.state) == true,
      to.state != .sent || !(to.eventId ?? "").isEmpty
    else { throw ShareFailure.invalid }
  }

  private func reconcileLocked() throws -> [ShareManifest] {
    var result: [ShareManifest] = []
    let session = try readStagingSession()
    for file in try fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
    where file.pathExtension == "part" {
      try fm.removeItem(at: file)
    }
    for directory in try directories() {
      var manifest: ShareManifest
      do { manifest = try load(directory.lastPathComponent) } catch ShareFailure.missingFile {
        // A killed create may leave a directory without a published manifest.
        try fm.removeItem(at: directory)
        continue
      } catch {
        Self.log.error(
          "share_metadata_unavailable share_id=\(directory.lastPathComponent, privacy: .public)")
        result.append(unavailable(directory.lastPathComponent))
        continue
      }
      let active = manifest.owner != nil && (manifest.leaseUntil ?? 0) > now()
      if (session != nil && manifest.session != session?.generation)
        || (!active && manifest.createdAt + ShareLimits.retention <= now())
      {
        try fm.removeItem(at: directory)
        continue
      }
      if !active {
        manifest.owner = nil
        manifest.leaseUntil = nil
        if manifest.operation.state == .dispatching { manifest.operation.state = .unknown }
        if manifest.operation.state == .uploading { manifest.operation.state = .ready }
        for index in manifest.files.indices {
          if manifest.files[index].operation?.state == .dispatching {
            manifest.files[index].operation?.state = .unknown
          }
          if manifest.files[index].operation?.state == .uploading {
            manifest.files[index].operation?.state = .ready
          }
        }
        let retained = Set(manifest.files.map(\.id) + ["manifest.json"])
        let stagedFiles = try fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)
        if stagedFiles.contains(where: { $0.pathExtension == "reservation" })
          && !manifest.importFailures.contains("incomplete_import")
        {
          manifest.importFailures.append("incomplete_import")
        }
        for file in stagedFiles {
          if !retained.contains(file.lastPathComponent) { try fm.removeItem(at: file) }
        }
        try save(manifest)
      }
      result.append(manifest)
    }
    return result.sorted { $0.createdAt < $1.createdAt }
  }

  private func reservedBytes() throws -> Int {
    try directories().reduce(0) { $0 + (try reservations(in: $1)) }
  }
  private func usedBytes(_ records: [ShareManifest]) throws -> Int {
    var total = try reservedBytes() + 16 * 1024  // descriptor/intents and atomic root writes
    for record in records {
      total += record.bytes + ShareLimits.metadataReservation
      let referenced = Set(record.files.map(\.id) + ["manifest.json"])
      for file in try fm.contentsOfDirectory(
        at: directory(record.id), includingPropertiesForKeys: [.fileSizeKey])
      {
        // A killed rename/publish or failed unlink can leave a complete file
        // outside its manifest. Charge it until ownership reconciliation can
        // remove it. In-progress .part bytes are already covered by reservation.
        if !referenced.contains(file.lastPathComponent) && file.pathExtension != "part"
          && file.pathExtension != "reservation"
        {
          total += try file.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
        }
      }
    }
    return total
  }
  private func reservations(in directory: URL) throws -> Int {
    try fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)
      .filter { $0.pathExtension == "reservation" }
      .reduce(0) { $0 + (try decode(Int.self, from: $1)) }
  }
  private func readSession() throws -> ShareSession? {
    if fm.fileExists(atPath: root.appendingPathComponent("session.pending.json").path) {
      return nil
    }
    let url = root.appendingPathComponent("session.json")
    return fm.fileExists(atPath: url.path) ? try decode(ShareSession.self, from: url) : nil
  }
  /// A failed credential write removes dispatch authority, while its durable
  /// descriptor intent still permits bounded staging under the same identity.
  private func readStagingSession() throws -> ShareSession? {
    let pending = root.appendingPathComponent("session.pending.json")
    return fm.fileExists(atPath: pending.path)
      ? try decode(ShareSession.self, from: pending) : try readSession()
  }
  private func unavailable(_ id: String) -> ShareManifest {
    var record = ShareManifest(
      id: id, createdAt: now(), session: "unavailable",
      text:
        "Saved share metadata is unavailable. Inspect the destination before explicitly discarding this record."
    )
    record.operation.state = .unknown
    record.operation.id = id
    record.importFailures = ["unreadable_manifest"]
    return record
  }
  private func readAll() throws -> [ShareManifest] {
    try directories().map { (try? load($0.lastPathComponent)) ?? unavailable($0.lastPathComponent) }
  }
  private func directories() throws -> [URL] {
    try fm.contentsOfDirectory(
      at: root, includingPropertiesForKeys: [.isDirectoryKey, .isSymbolicLinkKey]
    )
    .filter {
      guard UUID(uuidString: $0.lastPathComponent) != nil else { return false }
      let values = try $0.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
      return values.isDirectory == true && values.isSymbolicLink != true
    }
  }
  private func directory(_ id: String) throws -> URL {
    guard UUID(uuidString: id) != nil else { throw ShareFailure.invalid }
    let url = root.appendingPathComponent(id)
    if fm.fileExists(atPath: url.path),
      (try url.resourceValues(forKeys: [.isSymbolicLinkKey])).isSymbolicLink == true
    {
      throw ShareFailure.invalid
    }
    return url
  }
  private func load(_ id: String) throws -> ShareManifest {
    let value = try decode(
      ShareManifest.self, from: directory(id).appendingPathComponent("manifest.json"))
    guard value.version == 1, value.id == id, value.text.utf8.count <= ShareLimits.textBytes,
      value.files.count <= ShareLimits.files + 16,
      value.files.allSatisfy({
        UUID(uuidString: $0.id) != nil && $0.bytes >= 0 && $0.bytes <= ShareLimits.batchBytes
      }),
      (value.previews?.count ?? 0) <= 8
    else { throw ShareFailure.invalid }
    return value
  }
  private func save(_ manifest: ShareManifest) throws {
    try write(manifest, to: directory(manifest.id).appendingPathComponent("manifest.json"))
  }
  private func decode<T: Decodable>(_ type: T.Type, from url: URL) throws -> T {
    let fd = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
    guard fd >= 0 else {
      throw errno == ENOENT ? ShareFailure.missingFile : ShareFailure.unavailable
    }
    defer { close(fd) }
    var info = stat()
    guard fstat(fd, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG,
      info.st_size <= ShareLimits.manifestBytes, info.st_size >= 0
    else { throw ShareFailure.invalid }
    let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: false)
    return try JSONDecoder().decode(type, from: handle.readData(ofLength: Int(info.st_size)))
  }
  private func write<T: Encodable>(_ value: T, to url: URL) throws {
    let data = try ShareJSON.encode(value)
    guard data.count <= ShareLimits.manifestBytes else { throw ShareFailure.quota }
    try writeBytes(data, to: url)
  }
  private func writeBytes(_ data: Data, to url: URL) throws {
    let temporary = url.deletingLastPathComponent().appendingPathComponent(
      UUID().uuidString + ".part")
    let fd = open(temporary.path, O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW, 0o600)
    guard fd >= 0 else { throw ShareFailure.unavailable }
    defer {
      close(fd)
      try? fm.removeItem(at: temporary)
    }
    try protect(temporary)
    try data.withUnsafeBytes { raw in
      var offset = 0
      while offset < data.count {
        let count = Darwin.write(fd, raw.baseAddress!.advanced(by: offset), data.count - offset)
        guard count > 0 else { throw ShareFailure.unavailable }
        offset += count
      }
    }
    guard fsync(fd) == 0, rename(temporary.path, url.path) == 0 else {
      throw ShareFailure.unavailable
    }
    let directory = open(url.deletingLastPathComponent().path, O_RDONLY | O_NOFOLLOW)
    guard directory >= 0 else { throw ShareFailure.unavailable }
    defer { close(directory) }
    guard fsync(directory) == 0 else { throw ShareFailure.unavailable }
  }
  private func protect(_ url: URL) throws {
    var target = url
    var values = URLResourceValues()
    values.isExcludedFromBackup = true
    try target.setResourceValues(values)
    #if os(iOS)
      try fm.setAttributes(
        [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication],
        ofItemAtPath: url.path)
    #endif
  }
}
