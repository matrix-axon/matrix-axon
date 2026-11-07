import Foundation
import ImageIO
import XCTest

@testable import AxonIncomingShare

final class TextShareTests: XCTestCase {
  func testPortablePreviewEligibilityMatchesOldFoundationConstraints() {
    for url in [
      "https://example.org/é", "https://example.org/a|b", "https://example.org/{a}",
      "https://example.org/a^b", "https://u:p@example.org/", "https://example.org/%XX",
    ] {
      XCTAssertFalse(TextShare.previewURL(url))
    }
    XCTAssertTrue(TextShare.previewURL("https://example.org/%C3%A9?q=a%7Cb"))
  }
  func testPreviewCancellationBeforeWorkerStartAndSlotRecovery() throws {
    let requests = SharePreviewRequests()
    let first = UUID().uuidString
    let cancellation = try requests.begin(first)
    XCTAssertThrowsError(try requests.begin(UUID().uuidString)) {
      XCTAssertEqual($0 as? ShareFailure, .previewBusy)
    }
    requests.cancel(first)
    XCTAssertThrowsError(try cancellation.check()) {
      XCTAssertEqual($0 as? ShareFailure, .cancelled)
    }
    requests.finish(first)
    let second = try requests.begin(UUID().uuidString)
    XCTAssertNoThrow(try second.check())
    requests.cancel()
    XCTAssertThrowsError(try second.check())
  }
  private var root: URL!
  private var store: ShareStore!
  private var session: ShareSession!
  private let account = "00000000-0000-4000-8000-000000000001"
  private let room = "!test:example.org"
  private let link = "https://example.org/"
  override func setUpWithError() throws {
    root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    store = try ShareStore(root: root)
    session = ShareSession(
      installation: "test", server: "https://axon.example.org", credentialAvailable: true,
      expiresAt: nil)
    try store.publish(session, credential: {})
  }
  override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }
  private func draft(text: String? = nil) throws -> ShareManifest {
    var value = try store.claim(store.create(text: text ?? link).id)
    value.accountId = account
    value.roomId = room
    return try store.update(value)
  }
  private func response(_ status: Int, _ value: [String: Any]) throws -> (Int, Data) {
    (status, try JSONSerialization.data(withJSONObject: value))
  }
  private func destinations() throws -> (Int, Data) {
    try response(
      200,
      [
        "data": [
          [
            "account_id": account, "account_user_id": "@test:example.org", "room_id": room,
            "name": "Test",
          ]
        ]
      ])
  }
  private func sender(
    credential: (() throws -> (ShareSession, String))? = nil,
    _ request: @escaping TextShareSender.Request
  ) -> TextShareSender {
    TextShareSender(
      store: store, credential: credential ?? { (self.session, "synthetic-token") },
      request: request)
  }
  func testDiscardBetweenRoomCheckAndDispatchFencesTheSend() throws {
    let value = try draft()
    var sends = 0
    let sender = sender { _, _, path, _, _, _, _, _ in
      if path.hasPrefix("/v1/rooms?") {
        try self.store.discard(value.id)
        return try self.destinations()
      }
      sends += 1
      return try self.response(200, ["data": ["event_id": "$unexpected"]])
    }
    XCTAssertThrowsError(try sender.send(value))
    XCTAssertTrue(try store.list().isEmpty)
    XCTAssertEqual(sends, 0)
  }

  func testDiscardAfterDispatchIntentKeepsTheResultRecord() throws {
    let value = try draft()
    var sends = 0
    let sender = sender { _, _, path, _, _, _, _, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      sends += 1
      return try self.response(200, ["data": ["event_id": "$confirmed"]])
    }
    let sent = try sender.send(value) { checkpoint in
      if checkpoint.operation.state == .dispatching {
        do {
          try self.store.discard(value.id)
          XCTFail("A live dispatch must keep its outcome record")
        } catch {}
      }
    }
    XCTAssertEqual(sends, 1)
    XCTAssertEqual(sent.operation.eventId, "$confirmed")
    XCTAssertEqual(try store.list().first?.operation.state, .sent)
  }

  func testNormalizationPreservesProseAndDeduplicatesRepresentations() throws {
    XCTAssertEqual(
      try TextShare.normalize(text: ["https://EXAMPLE.org:443", "https://example.org/"], urls: []),
      "https://EXAMPLE.org:443")
    XCTAssertEqual(
      try TextShare.normalize(
        text: ["Read https://EXAMPLE.org:443", "https://example.org/"],
        urls: ["https://example.org/"]),
      "Read https://EXAMPLE.org:443")
    XCTAssertEqual(
      try TextShare.normalize(text: ["Useful context " + link, link, link], urls: [link]),
      "Useful context " + link)
    XCTAssertEqual(
      try TextShare.normalize(text: ["Useful context"], urls: [link, link]),
      "Useful context\n\n" + link)
    XCTAssertThrowsError(
      try TextShare.normalize(text: [String(repeating: "😀", count: 17000)], urls: []))
    for url in [
      "file:///private/a", "https://user:pass@example.org/", "https://", "javascript:alert(1)",
    ] {
      XCTAssertThrowsError(try TextShare.normalize(text: [], urls: [url]))
    }
  }
  func testUnselectedDraftCanFetchPickerWithoutDispatch() throws {
    let value = try store.claim(store.create(text: link).id)
    var paths: [String] = []
    let sender = sender { _, _, path, method, _, _, _, _ in
      paths.append(path)
      XCTAssertEqual(method, "GET")
      return try self.destinations()
    }
    XCTAssertEqual(try sender.destinations(value).count, 1)
    XCTAssertEqual(paths, ["/v1/rooms"])
    XCTAssertEqual(try store.list().first?.operation.state, .ready)
    try store.discard(value.id, owner: value.owner)
    XCTAssertTrue(try store.list().isEmpty)
  }
  func testAccountLookupIsBoundedAndIndependentOfJoinedRooms() throws {
    let value = try store.claim(store.create(text: link).id)
    var paths: [String] = []
    let sender = sender { _, _, path, method, body, _, _, limit in
      paths.append(path)
      XCTAssertEqual(method, "GET")
      XCTAssertNil(body)
      XCTAssertEqual(limit, 2 * 1024 * 1024)
      return try self.response(
        200,
        [
          "data": [
            ["account_id": self.account, "user_id": "@test:example.org", "state": "active"],
            [
              "account_id": UUID().uuidString, "user_id": "@out:example.org",
              "state": "deactivated",
            ],
            ["account_id": UUID().uuidString, "user_id": "@gone:example.org", "state": "deleting"],
          ]
        ])
    }
    let accounts = try sender.accounts(value)
    XCTAssertEqual(accounts.map(\.account_id), [account])
    XCTAssertEqual(accounts.first?.user_id, "@test:example.org")
    XCTAssertEqual(paths, ["/v1/accounts"])
    XCTAssertNil(try store.list().first?.accountId)
  }
  func testAccountLookupDoesNotTreatInvalidOrFailedResponsesAsNoAccounts() throws {
    let value = try store.claim(store.create(text: link).id)
    let valid = ["account_id": account, "user_id": "@test:example.org", "state": "active"]
    for data: [[String: String]] in [
      [["account_id": account]],
      [["account_id": "not-a-uuid", "user_id": "@test:example.org", "state": "active"]],
      [["account_id": account, "user_id": "", "state": "active"]],
      [["account_id": account, "user_id": "@test:example.org", "state": "unexpected"]],
      [valid, valid],
    ] {
      let sender = sender { _, _, _, _, _, _, _, _ in
        try self.response(200, ["data": data])
      }
      XCTAssertThrowsError(try sender.accounts(value)) {
        XCTAssertEqual($0 as? ShareFailure, .invalidResponse)
      }
    }
    for status in [401, 503] {
      let sender = sender { _, _, _, _, _, _, _, _ in try self.response(status, ["data": []]) }
      XCTAssertThrowsError(try sender.accounts(value)) {
        if status == 401 {
          XCTAssertEqual($0 as? ShareFailure, .authentication)
        } else {
          XCTAssertEqual(($0 as? ShareRequestFailure)?.status, status)
        }
      }
    }
    let empty = sender { _, _, _, _, _, _, _, _ in try self.response(200, ["data": []]) }
    XCTAssertEqual(try empty.accounts(value), [])
  }
  func testDispatchIntentPrecedesRequestAndSuccessRecordsEvent() throws {
    let value = try draft()
    var sends = 0
    let sender = sender { _, _, path, _, body, _, _, _ in
      if path.hasPrefix("/v1/rooms") {
        XCTAssertEqual(path, "/v1/rooms?account_id=" + self.account)
        return try self.destinations()
      }
      sends += 1
      XCTAssertTrue(path.hasSuffix("/rooms/%21test%3Aexample%2Eorg/send"))
      XCTAssertEqual(try self.store.list().first?.operation.state, .dispatching)
      XCTAssertEqual(
        (try JSONSerialization.jsonObject(with: body!)) as? NSDictionary,
        TextShare.wire(value) as NSDictionary)
      return try self.response(200, ["data": ["event_id": "$confirmed"]])
    }
    let sent = try sender.send(value)
    XCTAssertEqual(sent.operation.state, .sent)
    XCTAssertEqual(sent.operation.eventId, "$confirmed")
    XCTAssertThrowsError(try sender.send(sent))
    XCTAssertEqual(sends, 1)
  }
  func testResponseLossAndUntrustedErrorsAreUnknownAndNeverReplayed() throws {
    for status in [0, 200, 400, 500, 504] {
      let value = try draft()
      var sends = 0
      let sender = sender { _, _, path, _, _, _, _, _ in
        if path.hasPrefix("/v1/rooms") { return try self.destinations() }
        sends += 1
        if status == 0 { throw ShareFailure.expired }
        return try self.response(status, ["error": ["code": "unrecognized"]])
      }
      let unknown = try sender.send(value)
      XCTAssertEqual(unknown.operation.state, .unknown)
      XCTAssertThrowsError(try sender.send(unknown))
      XCTAssertEqual(sends, 1)
      XCTAssertEqual(unknown.text, link)
      try store.discard(unknown.id, owner: unknown.owner)
    }
  }
  func testExplicitSizeAndAuthRejectionsAreDefiniteAndEditable() throws {
    for (status, code) in [
      (413, "event_too_large"), (413, "preview_too_large"), (401, "unauthorized"),
      (404, "not_found"),
    ] {
      let value = try draft()
      let sender = sender { _, _, path, _, _, _, _, _ in
        if path.hasPrefix("/v1/rooms") { return try self.destinations() }
        return try self.response(status, ["error": ["code": code]])
      }
      var failed = try sender.send(value)
      XCTAssertEqual(failed.operation.state, .failed)
      XCTAssertEqual(failed.operation.failure, code)
      failed.text += " Edited"
      failed.operation.state = .ready
      XCTAssertNoThrow(try store.update(failed))
      try store.discard(value.id, owner: value.owner)
    }
  }
  func testStaleDestinationMissingAuthenticationAndOversizeNeverDispatch() throws {
    let value = try draft()
    var sends = 0
    let stale = sender { _, _, path, _, _, _, _, _ in
      if !path.hasPrefix("/v1/rooms") { sends += 1 }
      return try self.response(200, ["data": []])
    }
    XCTAssertThrowsError(try stale.send(value))
    let unavailable = sender(credential: { throw ShareFailure.authentication }) {
      _, _, _, _, _, _, _, _ in
      sends += 1
      throw ShareFailure.unavailable
    }
    XCTAssertThrowsError(try unavailable.send(value))
    var large = value
    large.text = String(repeating: "\\\"😀", count: 7000)
    large = try store.update(large)
    XCTAssertEqual(TextShare.validation(large), "event_too_large")
    XCTAssertThrowsError(try stale.send(large))
    XCTAssertEqual(sends, 0)
    XCTAssertEqual(try store.list().first?.operation.state, .ready)
  }
  func testUnknownGenerationAndMaliciousPreviewStayPlainAndNeverFetchOnArrival() throws {
    let value = try draft()
    var requests = 0
    let sender = sender { _, _, path, _, _, _, _, _ in
      requests += 1
      XCTAssertTrue(path.hasSuffix("/link-previews/generate"))
      return try self.response(200, ["data": ["status": "disabled", "thumbnail_status": "absent"]])
    }
    XCTAssertEqual(requests, 0)
    XCTAssertThrowsError(try sender.generate(value, url: link))
    XCTAssertNil(try store.list().first?.previews)
    XCTAssertEqual(requests, 1)
    let malicious = self.sender { _, _, _, _, _, _, _, _ in
      try self.response(
        200,
        [
          "data": [
            "status": "ready", "thumbnail_status": "absent",
            "preview": ["matched_url": "https://evil.example/", "title": "wrong target"],
          ]
        ])
    }
    XCTAssertThrowsError(try malicious.generate(value, url: link))
    XCTAssertNil(try store.list().first?.previews)
  }
  func testPreviewFallbackPreservesEachReasonAndKeepsOriginalLinkSendable() throws {
    for fallback in SharePreviewFallback.allCases {
      let value = try draft()
      var requests = 0
      let sender = sender { _, _, path, _, _, _, _, _ in
        requests += 1
        XCTAssertTrue(path.hasSuffix("/link-previews/generate"))
        return try self.response(
          200, ["data": ["status": fallback.rawValue, "thumbnail_status": "absent"]])
      }
      XCTAssertThrowsError(try sender.generate(value, url: link)) {
        XCTAssertEqual($0 as? SharePreviewFallback, fallback)
      }
      XCTAssertEqual(requests, 1)
      let saved = try XCTUnwrap(store.list().first(where: { $0.id == value.id }))
      XCTAssertEqual(saved.text, value.text)
      XCTAssertNil(saved.previews)
      XCTAssertNil(TextShare.validation(saved))
    }
  }
  func testMissingLocalImageRequiresExplicitRemoval() throws {
    var value = try draft()
    value.previews = [SharePreview(matched_url: link, image_upload_id: UUID().uuidString)]
    value = try store.update(value)
    var sends = 0
    let sender = sender { _, _, path, _, _, _, _, _ in
      if !path.hasPrefix("/v1/rooms") { sends += 1 }
      return try self.destinations()
    }
    XCTAssertThrowsError(try sender.send(value)) {
      XCTAssertEqual($0 as? ShareFailure, .missingFile)
    }
    XCTAssertEqual(sends, 0)
    XCTAssertEqual(try store.list().first?.operation.state, .ready)
  }

  func testRetainedThumbnailGetsFreshAccountHandleWithoutRegeneration() throws {
    var value = try draft()
    let png = Data(
      base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
    )!
    let expired = UUID().uuidString
    let fresh = UUID().uuidString
    value = try store.importThumbnail(
      value.id, owner: value.owner!, data: png, mime: "image/png",
      preview: SharePreview(matched_url: link, title: "Retained source", image_upload_id: expired))
    var paths: [String] = []
    let sender = sender { _, _, path, method, body, file, mime, _ in
      paths.append(path)
      if path.hasPrefix("/v1/rooms") { return try self.destinations() }
      if path.contains("/media/uploads?") {
        XCTAssertEqual(method, "POST")
        XCTAssertEqual(mime, "image/png")
        XCTAssertEqual(try Data(contentsOf: file!), png)
        return try self.response(200, ["data": ["upload_id": fresh]])
      }
      let wire = try JSONSerialization.jsonObject(with: body!) as! [String: Any]
      let previews = wire["url_previews"] as! [[String: Any]]
      XCTAssertEqual(previews[0]["image_upload_id"] as? String, fresh)
      XCTAssertEqual(try self.store.list().first?.previews?.first?.image_upload_id, fresh)
      return try self.response(200, ["data": ["event_id": "$card"]])
    }
    let sent = try sender.send(value)
    XCTAssertEqual(sent.operation.state, .sent)
    XCTAssertEqual(paths.count, 3)
    XCTAssertFalse(paths.contains(where: { $0.contains("generate") }))
    XCTAssertEqual(sent.files.filter { !$0.thumbnail }.count, 0)
    XCTAssertEqual(sent.previews?.first?.thumbnail, value.files[0].id)
  }

  func testGeneratedImageIsProtectedAndBoundToExactURL() throws {
    let value = try draft()
    let png = Data(
      base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
    )!
    let upload = UUID().uuidString
    var calls = 0
    let sender = sender { _, _, path, _, _, _, _, limit in
      calls += 1
      if path.hasSuffix("/generate") {
        return try self.response(
          200,
          [
            "data": [
              "status": "ready", "thumbnail_status": "ready",
              "preview": [
                "matched_url": self.link, "title": "Generated", "image_upload_id": upload,
              ],
            ]
          ])
      }
      XCTAssertTrue(path.hasSuffix("/media/uploads/" + upload + "/preview"))
      XCTAssertEqual(limit, ShareLimits.thumbnailBytes)
      return (200, png)
    }
    var imageState: SharePreviewImageState?
    let generated = try sender.generate(value, url: link, imageState: { imageState = $0 })
    XCTAssertEqual(imageState, .ready)
    XCTAssertEqual(calls, 2)
    XCTAssertEqual(generated.previews?.first?.provenance, "generated")
    XCTAssertEqual(generated.previews?.first?.thumbnail, generated.files.first?.id)
    XCTAssertTrue(generated.files.first?.thumbnail == true)
    XCTAssertNil(TextShare.validation(generated))
  }

  func testGeneratedTextCardReportsAbsentOrUnavailableImageWithoutFetchingOne() throws {
    for state in [SharePreviewImageState.absent, .unavailable] {
      let value = try draft()
      var calls = 0
      let sender = sender { _, _, path, _, _, _, _, _ in
        calls += 1
        XCTAssertTrue(path.hasSuffix("/generate"))
        return try self.response(
          200,
          [
            "data": [
              "status": "ready", "thumbnail_status": state.rawValue,
              "preview": ["matched_url": self.link, "title": "Text card"],
            ]
          ])
      }
      var reported: SharePreviewImageState?
      let generated = try sender.generate(value, url: link, imageState: { reported = $0 })
      XCTAssertEqual(reported, state)
      XCTAssertEqual(calls, 1)
      XCTAssertEqual(generated.text, value.text)
      XCTAssertEqual(generated.previews?.first?.title, "Text card")
      XCTAssertNil(generated.previews?.first?.image_upload_id)
      XCTAssertTrue(generated.files.isEmpty)
      XCTAssertNil(TextShare.validation(generated))
    }
  }

  func testCancellationBeforeAndAfterDispatchHasHonestOutcomes() throws {
    var value = try draft()
    let cancellation = ShareCancellation()
    var sends = 0
    let sender = TextShareSender(
      store: store, credential: { (self.session, "synthetic-token") }, cancellation: cancellation
    ) { _, _, path, _, _, _, _, _ in
      if path.hasPrefix("/v1/rooms") {
        cancellation.cancel()
        return try self.destinations()
      }
      sends += 1
      throw ShareFailure.unavailable
    }
    XCTAssertThrowsError(try sender.send(value))
    XCTAssertEqual(sends, 0)
    XCTAssertEqual(try store.list().first?.operation.state, .ready)
    try store.discard(value.id, owner: value.owner)
    value = try draft()
    let after = ShareCancellation()
    let inFlight = TextShareSender(
      store: store, credential: { (self.session, "synthetic-token") }, cancellation: after
    ) { _, _, path, _, _, _, _, _ in
      if path.hasPrefix("/v1/rooms") { return try self.destinations() }
      after.cancel()
      throw ShareFailure.expired
    }
    XCTAssertEqual(try inFlight.send(value).operation.state, .unknown)
  }
  func testFailedCompletionPublicationRetainsDispatchCheckpointForCaller() throws {
    let value = try draft()
    var checkpoint = value
    var dispatched = false
    let sender = sender { _, _, path, _, _, _, _, _ in
      if path.hasPrefix("/v1/rooms") { return try self.destinations() }
      dispatched = true
      // Identity revocation can race a successful remote response. A caller
      // must never restore its old ready state or claim that nothing was sent.
      try self.store.invalidateSession()
      return try self.response(200, ["data": ["event_id": "$accepted"]])
    }
    XCTAssertThrowsError(try sender.send(value, checkpoint: { checkpoint = $0 }))
    XCTAssertTrue(dispatched)
    XCTAssertEqual(checkpoint.operation.state, .dispatching)
  }
  private func photos(_ count: Int = 3) throws -> ShareManifest {
    var value = try draft()
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source)
    for index in 0..<count {
      value = try store.importFile(
        value.id, owner: value.owner!, source: source,
        name: "photo-\(index).png", mime: "image/png", photo: true)
    }
    value.previews = [SharePreview(matched_url: link, title: "Caption card")]
    return try store.update(value)
  }

  func testPhotosSendTheirOwnCaptionsWithOnlyTheFirstCard() throws {
    var value = try photos()
    value.version = 3
    value.files[1].caption = "Second photo: café 🌊\nA different caption"
    value.files[2].caption = ""
    value = try store.update(value)
    value = try store.importThumbnail(
      value.id, owner: value.owner!,
      data: Data(contentsOf: root.appendingPathComponent("photo.png")), mime: "image/png",
      preview: SharePreview(matched_url: link, title: "Caption card"))
    var uploads: [Data] = []
    var messages: [[String: Any]] = []
    let sender = sender { _, _, path, _, body, file, mime, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if let file {
        XCTAssertEqual(mime, "image/png")
        uploads.append(try Data(contentsOf: file))
        return try self.response(200, ["data": ["upload_id": UUID().uuidString]])
      }
      XCTAssertTrue(path.hasSuffix("/send-media"))
      let saved = try self.store.list()[0]
      XCTAssertEqual(saved.files[messages.count].operation?.state, .dispatching)
      messages.append(try JSONSerialization.jsonObject(with: body!) as! [String: Any])
      return try self.response(200, ["data": ["event_id": "$photo-\(messages.count)"]])
    }
    let result = try sender.send(value)
    XCTAssertTrue(result.completed)
    XCTAssertEqual(uploads.count, 4)
    XCTAssertEqual(messages.map { $0["caption"] as? String }, [link, "Second photo: café 🌊\nA different caption", ""])
    XCTAssertEqual((messages[0]["url_previews"] as? [Any])?.count, 1)
    XCTAssertNotNil((messages[0]["url_previews"] as? [[String: Any]])?[0]["image_upload_id"])
    XCTAssertEqual((messages[1]["url_previews"] as? [Any])?.count, 0)
    XCTAssertEqual(
      result.files.compactMap { $0.operation?.eventId }, ["$photo-1", "$photo-2", "$photo-3"])
  }

  func testPhotoResponseLossStopsTheBatchAndExplicitRecoverySkipsConfirmedCaption() throws {
    var value = try photos()
    value.version = 3
    value.files[1].caption = "Second caption"
    value.files[2].caption = "Third caption"
    value = try store.update(value)
    var sends = 0
    let sender = sender { _, _, path, _, _, file, _, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if file != nil { return try self.response(200, ["data": ["upload_id": UUID().uuidString]]) }
      sends += 1
      if sends == 2 { throw ShareFailure.expired }
      return try self.response(200, ["data": ["event_id": "$first"]])
    }
    var result = try sender.send(value)
    XCTAssertEqual(result.files.compactMap { $0.operation?.state }, [.sent, .unknown, .ready])
    XCTAssertThrowsError(try sender.send(result))
    XCTAssertEqual(sends, 2)
    result.files[1].operation!.state = .ready  // Explicit duplicate-risk consent in recovery.
    result = try store.update(result)
    var retried: [[String: Any]] = []
    let recovery = self.sender { _, _, path, _, body, file, _, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if file != nil { return try self.response(200, ["data": ["upload_id": UUID().uuidString]]) }
      retried.append(try JSONSerialization.jsonObject(with: body!) as! [String: Any])
      return try self.response(200, ["data": ["event_id": "$remaining-\(retried.count)"]])
    }
    result = try recovery.send(result)
    XCTAssertTrue(result.completed)
    XCTAssertEqual(result.files[0].operation?.eventId, "$first")
    XCTAssertEqual(retried.count, 2)
    XCTAssertEqual(retried.map { $0["caption"] as? String }, ["Second caption", "Third caption"])
    XCTAssertTrue(retried.allSatisfy { ($0["url_previews"] as? [Any])?.isEmpty == true })
  }

  func testPhotoUploadFailureIsRetryableAndRetainsEarlierConfirmation() throws {
    let value = try photos()
    var uploaded = 0
    let sender = sender { _, _, path, _, _, file, _, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if file != nil {
        uploaded += 1
        if uploaded == 2 { throw ShareFailure.expired }
        return try self.response(200, ["data": ["upload_id": UUID().uuidString]])
      }
      return try self.response(200, ["data": ["event_id": "$first"]])
    }
    XCTAssertThrowsError(try sender.send(value))
    let saved = try store.list()[0]
    XCTAssertEqual(saved.files.compactMap { $0.operation?.state }, [.sent, .failed, .ready])
    XCTAssertEqual(saved.files[1].operation?.failure, "upload_failed")
    XCTAssertFalse(saved.completed)
    try store.cancelComposer(saved)
    let recovered = try XCTUnwrap(try store.list().first)
    XCTAssertNil(recovered.owner)
    XCTAssertEqual(recovered.files[0].operation?.eventId, "$first")
  }

  func testCancellationBetweenPhotoMessagesKeepsTheRemainingFilesForRecovery() throws {
    let value = try photos()
    let cancellation = ShareCancellation()
    var sends = 0
    let sender = TextShareSender(
      store: store, credential: { (self.session, "synthetic-token") }, cancellation: cancellation
    ) { _, _, path, _, _, file, _, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if file != nil { return try self.response(200, ["data": ["upload_id": UUID().uuidString]]) }
      sends += 1
      cancellation.cancel()
      return try self.response(200, ["data": ["event_id": "$first"]])
    }
    XCTAssertThrowsError(try sender.send(value))
    let saved = try store.list()[0]
    XCTAssertEqual(sends, 1)
    try store.cancelComposer(saved)
    let recovered = try XCTUnwrap(try store.list().first)
    XCTAssertNil(recovered.owner)
    XCTAssertEqual(recovered.files.compactMap { $0.operation?.state }, [.sent, .ready, .ready])
  }

  func testNativeSenderUploadsReducedBytesWithMatchingMimeAndFilename() throws {
    let source = root.appendingPathComponent("photo.heic")
    try PhotoFixture.write(source, type: .heic, width: 2400, height: 1600)
    var value = try draft()
    value = try store.importFile(
      value.id, owner: value.owner!, source: source,
      name: "photo.heic", mime: "image/heic", photo: true)
    value = try store.setPhotoSize(value, size: .small)
    var uploaded = false
    let sender = sender { _, _, path, _, _, file, mime, _ in
      if path.hasPrefix("/v1/rooms?") { return try self.destinations() }
      if let file {
        XCTAssertEqual(mime, "image/jpeg")
        XCTAssertTrue(path.contains("filename=photo%2Ejpg"))
        let image = try XCTUnwrap(CGImageSourceCreateWithURL(file as CFURL, nil))
        let properties = try XCTUnwrap(
          CGImageSourceCopyPropertiesAtIndex(image, 0, nil) as? [CFString: Any])
        XCTAssertEqual(properties[kCGImagePropertyPixelWidth] as? Int, 640)
        uploaded = true
        return try self.response(200, ["data": ["upload_id": UUID().uuidString]])
      }
      return try self.response(200, ["data": ["event_id": "$reduced"]])
    }
    let result = try sender.send(value)
    XCTAssertTrue(uploaded)
    XCTAssertTrue(result.completed)
    XCTAssertEqual(result.files[0].mime, "image/heic")
    XCTAssertEqual(result.files[0].photoSize, .small)
  }

}
