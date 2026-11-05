import Foundation
import os

public final class ShareCancellation {
  private let lock = NSLock()
  private var cancelled = false
  public init() {}
  public func cancel() {
    lock.lock()
    cancelled = true
    lock.unlock()
  }
  public func check() throws {
    lock.lock()
    defer { lock.unlock() }
    if cancelled { throw ShareFailure.cancelled }
  }
}

/// One app preview at a time, independently of upload slots. Registration and
/// cancellation share a lock so cancellation can precede worker startup.
public final class SharePreviewRequests {
  private let lock = NSLock()
  private var requests: [String: ShareCancellation] = [:]
  public init() {}
  public func begin(_ id: String) throws -> ShareCancellation {
    guard UUID(uuidString: id) != nil else { throw ShareFailure.invalid }
    lock.lock()
    defer { lock.unlock() }
    guard requests.isEmpty else { throw ShareFailure.previewBusy }
    let cancellation = ShareCancellation()
    requests[id] = cancellation
    return cancellation
  }
  public func cancel(_ id: String? = nil) {
    lock.lock()
    defer { lock.unlock() }
    if let id {
      requests[id]?.cancel()
    } else {
      for request in requests.values { request.cancel() }
    }
  }
  public func finish(_ id: String) {
    lock.lock()
    defer { lock.unlock() }
    requests.removeValue(forKey: id)
  }
}

/// Pure intake normalization. Different representations of one URL are folded
/// together; substantive accompanying text is retained without fetching it.
public enum TextShare {
  private static let linkExpression = try! NSRegularExpression(
    pattern: "(?:https?://|matrix:)[^\\s\u{feff}<>\"]+")
  private static let portableURLCharacters = CharacterSet(
    charactersIn:
      "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~:/?#[]@!$&'()*+,;=%")
  private static let invalidPercent = try! NSRegularExpression(pattern: "%(?![0-9A-Fa-f]{2})")
  public static func previewField(_ value: String?, limit: Int) -> String? {
    guard let value, value.utf16.count <= limit else { return nil }
    let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
    return trimmed.isEmpty || trimmed.utf16.count > limit ? nil : trimmed
  }
  public static func normalize(text: [String], urls: [String]) throws -> String {
    guard text.count <= 32, urls.count <= 32 else { throw ShareFailure.quota }
    var pieces: [String] = []
    for part in text where !part.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
      guard part.utf8.count <= ShareLimits.textBytes else { throw ShareFailure.quota }
      if !pieces.contains(part) { pieces.append(part) }
    }
    pieces = pieces.enumerated().filter { index, part in
      let trimmed = part.trimmingCharacters(in: .whitespacesAndNewlines)
      guard validURL(trimmed) else { return true }
      // Prefer accompanying prose; among equivalent URL-only representations,
      // retain the first rather than allowing each to remove the other.
      return !pieces.enumerated().contains { otherIndex, other in
        otherIndex != index && matchedLink(trimmed, in: other) != nil
          && (otherIndex < index
            || !validURL(other.trimmingCharacters(in: .whitespacesAndNewlines)))
      }
    }.map(\.element)
    var result = pieces.joined(separator: "\n\n")
    for url in urls {
      guard validURL(url) else { throw ShareFailure.invalid }
      if matchedLink(url, in: result) == nil {
        result += (result.isEmpty ? "" : "\n\n") + url
      }
    }
    guard result.utf8.count <= ShareLimits.textBytes else { throw ShareFailure.quota }
    guard !result.isEmpty else { throw ShareFailure.invalid }
    return result
  }

  public static func validURL(_ raw: String) -> Bool {
    guard raw.utf16.count <= 4096,
      let url = URLComponents(string: raw),
      ["http", "https"].contains(url.scheme), let host = url.host, !host.isEmpty,
      url.user == nil, url.password == nil,
      raw.rangeOfCharacter(from: .whitespacesAndNewlines.union(.controlCharacters)) == nil
    else { return false }
    return true
  }
  public static func previewURL(_ raw: String) -> Bool {
    validURL(raw) && URLComponents(string: raw)?.scheme == "https"
      && raw.rangeOfCharacter(from: portableURLCharacters.inverted) == nil
      && invalidPercent.firstMatch(in: raw, range: NSRange(location: 0, length: raw.utf16.count))
        == nil
  }
  static func matchedLink(_ raw: String, in text: String) -> String? {
    let canonical = PreviewContract.canonicalURL(raw) ?? raw
    return links(text).first { (PreviewContract.canonicalURL($0) ?? $0) == canonical }
  }

  public static func links(_ text: String) -> [String] {
    let value = text as NSString
    return linkExpression.matches(in: text, range: NSRange(location: 0, length: value.length))
      .map {
        value.substring(with: $0.range).trimmingCharacters(
          in: CharacterSet(charactersIn: ".,;:!?)]'\""))
      }
      .filter(validURL)
  }

  public static func wire(_ manifest: ShareManifest, reserveImages: Bool = false) -> [String: Any] {
    let previews: [[String: Any]] = (manifest.previews ?? []).map { preview in
      var entry: [String: Any] = ["matched_url": preview.matched_url]
      entry["title"] = preview.title
      entry["description"] = preview.description
      entry["site_name"] = preview.site_name
      entry["image_upload_id"] =
        reserveImages && preview.thumbnail != nil
        ? "00000000-0000-4000-8000-000000000001" : preview.image_upload_id
      return entry
    }
    return ["body": manifest.text, "url_previews": previews]
  }

  public static func validation(_ manifest: ShareManifest) -> String? {
    guard !manifest.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
      return "empty_text"
    }
    for preview in manifest.previews ?? [] {
      if (preview.title?.utf16.count ?? 0) > 300 { return "preview_title_too_large" }
      if (preview.description?.utf16.count ?? 0) > 1000 { return "preview_description_too_large" }
      if (preview.site_name?.utf16.count ?? 0) > 120 { return "preview_site_name_too_large" }
    }
    return PreviewContract.check(wire(manifest, reserveImages: true))
  }

  public static func definiteRejection(status: Int, code: String?) -> Bool {
    let codes = [
      400: ["bad_request"], 401: ["unauthorized"], 403: ["forbidden"],
      404: ["not_found"], 413: ["event_too_large", "preview_too_large", "payload_too_large"],
    ]
    return code.map { codes[status]?.contains($0) == true } ?? false
  }
}

public struct ShareAccount: Decodable, Equatable {
  public let account_id: String
  public let user_id: String
  public let state: String
}

public struct ShareDestination: Decodable, Equatable {
  public let account_id: String
  public let account_user_id: String
  public let room_id: String
  public let name: String?
  public let canonical_alias: String?
  public let display_name: String?
  public let room_type: String?
  public var title: String { name ?? canonical_alias ?? display_name ?? room_id }
}

public enum SharePreviewImageState: String {
  case ready, absent, unavailable
}

public enum SharePreviewFallback: String, Error, CaseIterable {
  case unsupported, disabled, timeout, unavailable, busy
  case noMetadata = "no_metadata"

  public var explanation: String {
    switch self {
    case .unsupported: return "Your homeserver does not support this preview lookup."
    case .disabled: return "Your homeserver has disabled URL previews."
    case .timeout: return "The homeserver's preview lookup timed out."
    case .unavailable: return "The homeserver could not retrieve a preview for this URL."
    case .busy: return "The preview service is busy or rate limited. Wait before retrying."
    case .noMetadata: return "The homeserver returned no usable preview metadata for this URL."
    }
  }
}

/// All methods run on the extension's serial worker. The store's cross-process
/// owner/revision fences protect each checkpoint; network waits never hold it.
public final class TextShareSender {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "share-sender")
  public typealias Request = (ShareSession, String, String, String, Data?, URL?, String?, Int)
    throws -> (Int, Data)
  private let store: ShareStore
  private let credential: () throws -> (ShareSession, String)
  private let request: Request
  private let cancellation: ShareCancellation
  public init(
    store: ShareStore, credential: @escaping () throws -> (ShareSession, String),
    cancellation: ShareCancellation = ShareCancellation(), request: @escaping Request
  ) {
    self.store = store
    self.credential = credential
    self.request = request
    self.cancellation = cancellation
  }
  private func call(
    _ manifest: ShareManifest, _ path: String, method: String = "GET", body: Data? = nil,
    file: URL? = nil, mime: String? = nil, limit: Int = 2 * 1024 * 1024
  ) throws -> (Int, Data) {
    let (session, token) = try authorize(manifest)
    return try request(session, token, path, method, body, file, mime, limit)
  }
  private func authorize(_ manifest: ShareManifest) throws -> (ShareSession, String) {
    try cancellation.check()
    try store.checkTransport()
    let (session, token) = try credential()
    guard manifest.session == session.generation else { throw ShareFailure.stale }
    _ = try store.owned(manifest.id, owner: manifest.owner ?? "", account: manifest.accountId)
    return (session, token)
  }
  private func object(_ response: (Int, Data)) throws -> [String: Any] {
    guard (200..<300).contains(response.0),
      let envelope = try JSONSerialization.jsonObject(with: response.1) as? [String: Any],
      let data = envelope["data"] as? [String: Any]
    else { throw response.0 == 401 ? ShareFailure.authentication : ShareFailure.unavailable }
    return data
  }
  public func destinations(_ manifest: ShareManifest, selectedAccountOnly: Bool = false) throws
    -> [ShareDestination]
  {
    var path = "/v1/rooms"
    if selectedAccountOnly {
      guard let account = manifest.accountId, UUID(uuidString: account) != nil else {
        throw ShareFailure.invalid
      }
      path += "?account_id=" + account
    }
    let rooms: [ShareDestination] = try list(manifest, path, limit: ShareLimits.roomListBytes)
    return rooms.filter { $0.room_type != "m.space" }
  }
  /// Accounts are independent of rooms: an active account can have no joined
  /// rooms, and logged-out accounts must not be offered as sending identities.
  public func accounts(_ manifest: ShareManifest) throws -> [ShareAccount] {
    let accounts: [ShareAccount] = try list(manifest, "/v1/accounts", limit: 2 * 1024 * 1024)
    guard Set(accounts.map(\.account_id)).count == accounts.count,
      accounts.allSatisfy({
        UUID(uuidString: $0.account_id) != nil && !$0.user_id.isEmpty
          && ["active", "deactivated", "deleting"].contains($0.state)
      })
    else {
      Self.log.error("share_accounts_invalid_response")
      throw ShareFailure.invalidResponse
    }
    return accounts.filter { $0.state == "active" }
  }
  private struct ListEnvelope<Item: Decodable>: Decodable { var data: [Item] }
  private func list<Item: Decodable>(_ manifest: ShareManifest, _ path: String, limit: Int) throws
    -> [Item]
  {
    let response = try call(manifest, path, limit: limit)
    guard response.0 == 200 else {
      if response.0 == 401 { throw ShareFailure.authentication }
      throw ShareRequestFailure(status: response.0)
    }
    do {
      return try JSONDecoder().decode(ListEnvelope<Item>.self, from: response.1).data
    } catch {
      // Never log the decoder's description: it may quote response content.
      Self.log.error("share_list_invalid_response bytes=\(response.1.count)")
      throw ShareFailure.invalidResponse
    }
  }
  private func accountPath(_ manifest: ShareManifest) throws -> String {
    guard let account = manifest.accountId, UUID(uuidString: account) != nil else {
      throw ShareFailure.invalid
    }
    return "/v1/accounts/" + account
  }
  public func generate(
    _ input: ShareManifest, url: String,
    imageState: (SharePreviewImageState) -> Void = { _ in }
  ) throws -> ShareManifest {
    guard TextShare.links(input.text).contains(url),
      (input.previews?.count ?? 0) < 8
        || input.previews?.contains(where: { $0.matched_url == url }) == true
    else { throw ShareFailure.invalid }
    let path = try accountPath(input)
    let response = try call(
      input, path + "/link-previews/generate", method: "POST", body: ShareJSON.encode(["url": url]),
      mime: "application/json")
    let data = try object(response)
    guard PreviewContract.validateWire(data, schema: "GeneratedPreviewDto") else {
      throw ShareFailure.invalidResponse
    }
    if let status = data["status"] as? String, let fallback = SharePreviewFallback(rawValue: status)
    {
      Self.log.info("share_preview_fallback status=\(fallback.rawValue, privacy: .public)")
      throw fallback
    }
    guard data["status"] as? String == "ready", let wire = data["preview"] as? [String: Any],
      wire["matched_url"] as? String == url
    else { throw ShareFailure.invalidResponse }
    guard let state = (data["thumbnail_status"] as? String).flatMap(SharePreviewImageState.init),
      (state == .ready) == (wire["image_upload_id"] as? String != nil)
    else { throw ShareFailure.invalidResponse }
    Self.log.info("share_generated_preview image=\(state.rawValue, privacy: .public)")
    var preview = SharePreview(
      matched_url: url, title: wire["title"] as? String,
      description: wire["description"] as? String, site_name: wire["site_name"] as? String,
      image_upload_id: wire["image_upload_id"] as? String, provenance: "generated")
    if let upload = preview.image_upload_id {
      guard UUID(uuidString: upload) != nil else { throw ShareFailure.invalid }
      let image = try call(input, path + "/media/uploads/" + upload + "/preview")
      guard image.0 == 200 else { throw ShareFailure.unavailable }
      let mime = Self.imageMime(image.1)
      let retained = try store.importThumbnail(
        input.id, owner: input.owner!, data: image.1, mime: mime, preview: preview)
      imageState(.ready)
      return retained
    }
    preview.image_upload_id = nil
    var next = input
    next.previews = (next.previews ?? []).filter { $0.matched_url != url } + [preview]
    let saved = try store.update(next)
    imageState(state)
    return saved
  }
  public static func imageMime(_ data: Data) -> String {
    if data.starts(with: [137, 80, 78, 71]) { return "image/png" }
    if data.starts(with: [255, 216, 255]) { return "image/jpeg" }
    return "image/webp"
  }
  public func send(_ input: ShareManifest, checkpoint: (ShareManifest) -> Void = { _ in }) throws
    -> ShareManifest
  {
    guard input.operation.state == .ready, input.importFailures.isEmpty,
      input.files.allSatisfy(\.thumbnail), let room = input.roomId,
      TextShare.validation(input) == nil
    else { throw ShareFailure.invalid }
    let path = try accountPath(input)
    guard
      try destinations(input, selectedAccountOnly: true).contains(where: {
        $0.account_id == input.accountId && $0.room_id == room
      })
    else { throw ShareFailure.stale }
    var next = input
    // Explicit sends always restore a fresh account-owned handle from retained
    // bytes. Neither handle expiry nor a missing copy triggers regeneration.
    for index in (next.previews ?? []).indices {
      let preview = next.previews![index]
      if let thumbnail = preview.thumbnail {
        let uploaded = try store.withVerifiedFile(next.id, file: thumbnail, owner: next.owner!) {
          url, file in
          try self.object(
            self.call(
              next, path + "/media/uploads?kind=image&filename=preview", method: "POST", file: url,
              mime: file.mime))
        }
        guard let upload = uploaded["upload_id"] as? String, UUID(uuidString: upload) != nil else {
          throw ShareFailure.invalid
        }
        next.previews![index].image_upload_id = upload
        next = try store.update(next)
        checkpoint(next)
      } else if preview.image_upload_id != nil {
        throw ShareFailure.missingFile
      }
    }
    let body = try JSONSerialization.data(
      withJSONObject: TextShare.wire(next), options: [.sortedKeys, .withoutEscapingSlashes])
    // Validate credentials before persisting dispatch intent, then recheck the
    // session/owner immediately before the outbound request as well.
    _ = try authorize(next)
    next.operation.state = .dispatching
    next = try store.update(next)
    checkpoint(next)
    let authority: (ShareSession, String)
    do { authority = try authorize(next) } catch {
      next.operation.state = .failed
      next.operation.failure = "pre_dispatch_validation"
      return try store.update(next)
    }
    do {
      let encodedRoom = room.addingPercentEncoding(withAllowedCharacters: .alphanumerics)!
      let response = try request(
        authority.0, authority.1, path + "/rooms/" + encodedRoom + "/send", "POST", body, nil,
        "application/json", 256 * 1024)
      let envelope = (try? JSONSerialization.jsonObject(with: response.1)) as? [String: Any]
      let data = envelope?["data"] as? [String: Any]
      if response.0 == 200, let event = data?["event_id"] as? String, !event.isEmpty {
        next.operation.state = .sent
        next.operation.eventId = event
      } else {
        let code = (envelope?["error"] as? [String: Any])?["code"] as? String
        next.operation.state =
          TextShare.definiteRejection(status: response.0, code: code) ? .failed : .unknown
        next.operation.failure =
          next.operation.state == .failed ? (code ?? "definite_rejection") : "unconfirmed_dispatch"
      }
    } catch {
      // These adapter failures occur before URLSession resumes a task. A
      // timeout/unavailable response after resume remains ambiguous.
      let rejectedBeforeDispatch =
        (error as? ShareFailure).map {
          [ShareFailure.authentication, .httpsRequired, .invalid, .busy, .cancelled].contains($0)
        } ?? false
      next.operation.state = rejectedBeforeDispatch ? .failed : .unknown
      next.operation.failure =
        rejectedBeforeDispatch ? "pre_dispatch_validation" : "unconfirmed_dispatch"
    }
    // A failed completion write deliberately leaves the dispatch breadcrumb.
    return try store.update(next)
  }
}
