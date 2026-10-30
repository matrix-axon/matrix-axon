import Foundation
import XCTest

@testable import AxonIncomingShare

final class ShareStoreTests: XCTestCase {
  var root: URL!
  var time: TimeInterval = 1000
  var store: ShareStore!
  override func setUpWithError() throws {
    root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    store = try ShareStore(root: root, now: { self.time })
    try store.publish(
      ShareSession(
        installation: "install", server: "https://axon.example",
        credentialAvailable: true, expiresAt: nil), credential: {})
  }
  override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }

  func testThumbnailAttachmentPublishesAtomicallyAndReclaimsReplacedBytes() throws {
    let png = Data(
      base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
    )!
    var item = try store.claim(store.create(text: "draft").id)
    item.previews = (0..<8).map { SharePreview(matched_url: "https://example.org/" + String($0)) }
    item = try store.update(item)
    XCTAssertThrowsError(
      try store.importThumbnail(
        item.id, owner: item.owner!, data: png, mime: "image/png",
        preview: SharePreview(matched_url: "https://example.org/ninth")))
    XCTAssertEqual(try store.list().first?.files.count, 0)
    item = try store.importThumbnail(
      item.id, owner: item.owner!, data: png, mime: "image/png",
      preview: SharePreview(matched_url: "https://example.org/0"))
    let first = item.files[0].id
    for _ in 0..<20 {
      item = try store.importThumbnail(
        item.id, owner: item.owner!, data: png, mime: "image/png",
        preview: SharePreview(matched_url: "https://example.org/0"))
    }
    XCTAssertEqual(item.files.count, 1)
    XCTAssertEqual(item.previews?.count, 8)
    XCTAssertEqual(item.previews?.last?.thumbnail, item.files[0].id)
    XCTAssertFalse(
      FileManager.default.fileExists(
        atPath: root.appendingPathComponent(item.id).appendingPathComponent(first).path))
  }
  func testPrivateCleanupJournalSurvivesRestartAndBusySharedStorage() throws {
    let control = root.appendingPathComponent("../" + UUID().uuidString).standardizedFileURL
    defer { try? FileManager.default.removeItem(at: control) }
    let journal = try ShareStore(root: control)
    var item = try store.claim(store.create(text: "removed account").id)
    item.accountId = UUID().uuidString
    item = try store.update(item)
    _ = try store.create(text: "other account")
    try journal.recordCleanup(account: item.accountId!)
    try store.coordinated {
      XCTAssertThrowsError(try journal.reconcileCleanup(into: store))
    }
    let restarted = try ShareStore(root: control)
    try restarted.reconcileCleanup(into: store)
    XCTAssertEqual(try store.list().count, 1)
    XCTAssertThrowsError(try store.update(item))
    try restarted.recordCleanup()
    try ShareStore(root: control).reconcileCleanup(into: store)
    XCTAssertNil(try store.session())
    XCTAssertTrue(try store.list().isEmpty)
    // An already consumed journal is idempotent.
    try restarted.reconcileCleanup(into: store)
  }
  func testInactiveLegacyUnreferencedThumbnailIsReclaimed() throws {
    let png = Data(
      base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
    )!
    var item = try store.claim(store.create(text: "draft").id)
    item = try store.importThumbnail(item.id, owner: item.owner!, data: png, mime: "image/png")
    let file = root.appendingPathComponent(item.id).appendingPathComponent(item.files[0].id)
    XCTAssertEqual(try store.list().first?.files.count, 1)
    _ = try store.update(item, release: true)
    XCTAssertEqual(try store.list().first?.files.count, 0)
    XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))
  }
  func testListingUnchangedDraftDoesNotRewriteManifest() throws {
    let item = try store.create(text: "draft")
    let path = root.appendingPathComponent(item.id).appendingPathComponent("manifest.json").path
    let before =
      try FileManager.default.attributesOfItem(atPath: path)[.systemFileNumber] as! NSNumber
    _ = try store.list()
    let after =
      try FileManager.default.attributesOfItem(atPath: path)[.systemFileNumber] as! NSNumber
    XCTAssertEqual(before, after)
  }

  func testCompetingConsumersAndExpiredOwnerCannotCommit() throws {
    let item = try store.create(text: "hello")
    var owned = try store.claim(item.id)
    let second = try ShareStore(root: root, now: { self.time })
    XCTAssertThrowsError(try second.claim(item.id))
    time += ShareLimits.lease + 1
    let newOwner = try second.claim(item.id)
    XCTAssertNotEqual(owned.owner, newOwner.owner)
    owned.text = "late completion"
    XCTAssertThrowsError(try store.update(owned))
  }

  func testKilledDispatchIsUnknownAndCannotAutomaticallyRepeat() throws {
    var owned = try store.claim(store.create(text: "hello").id)
    owned.operation.state = .dispatching
    owned = try store.update(owned)
    time += ShareLimits.lease + 1
    XCTAssertEqual(try store.list().first?.operation.state, .unknown)
    owned = try store.claim(owned.id)
    owned.operation.state = .dispatching
    XCTAssertThrowsError(try store.update(owned))
  }

  func testSignOutFencesOldWriterAndRefreshPreservesDraft() throws {
    let owned = try store.claim(store.create(text: "hello").id)
    var session = try XCTUnwrap(store.session())
    session.revision = UUID().uuidString
    try store.publish(session, credential: {})
    XCTAssertEqual(try store.list().count, 1)
    session.generation = UUID().uuidString
    session.credentialAvailable = false
    try store.publish(session, credential: {})
    XCTAssertTrue(try store.list().isEmpty)
    XCTAssertThrowsError(try store.update(owned))
  }

  func testCredentialWriteFailureRemovesAuthority() throws {
    let session = try XCTUnwrap(store.session())
    XCTAssertThrowsError(try store.publish(session, credential: { throw ShareFailure.unavailable }))
    XCTAssertNil(try store.session())
  }

  func testReceivedIsNotAcknowledgedAndSentNeedsEventID() throws {
    var owned = try store.claim(store.create(text: "hello").id)
    XCTAssertThrowsError(try store.discard(owned.id, owner: owned.owner, completed: true))
    owned.operation.state = .dispatching
    owned = try store.update(owned)
    owned.operation.state = .sent
    XCTAssertThrowsError(try store.update(owned))
    owned.operation.eventId = "$confirmed"
    owned = try store.update(owned)
    try store.discard(owned.id, owner: owned.owner, completed: true)
    XCTAssertTrue(try store.list().isEmpty)
  }

  func testQuotaExpiryCannotBeExtendedByOwnership() throws {
    for _ in 0..<ShareLimits.pendingCount { _ = try store.create(text: "draft") }
    XCTAssertThrowsError(try store.create(text: "one too many"))
    let item = try store.claim(XCTUnwrap(store.list().first).id)
    time += ShareLimits.retention - 1
    var renewed = try store.claim(item.id)
    renewed = try store.update(renewed)
    time += 2
    XCTAssertTrue(try store.list().isEmpty)
    time += ShareLimits.lease
    XCTAssertTrue(try store.list().isEmpty)
  }

  func testPartialWriterAndTraversal() throws {
    let empty = root.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: false)
    XCTAssertTrue(try store.list().isEmpty)
    XCTAssertFalse(FileManager.default.fileExists(atPath: empty.path))
    XCTAssertThrowsError(try store.claim("../session.json"))
    XCTAssertThrowsError(try store.create(text: String(repeating: "😀", count: 17000)))
  }
  func testEscapedTextAtImportCeilingRemainsEditableButNotSendable() throws {
    let text = String(repeating: "\u{1}", count: ShareLimits.textBytes)
    let manifest = try store.create(text: text)
    XCTAssertEqual(try store.list().first?.text, text)
    XCTAssertEqual(try store.claim(manifest.id).text, text)
    XCTAssertEqual(PreviewContract.check(["body": text, "url_previews": []]), "event_too_large")
  }

  func testStreamingImportsPreserveOrderAndDetectCorruption() throws {
    let source = root.appendingPathComponent("provider.txt")
    try Data("original".utf8).write(to: source)
    var owned = try store.claim(store.create(text: "caption").id)
    for name in ["first.txt", "second.txt"] {
      owned = try store.importFile(
        owned.id, owner: owned.owner!, source: source, name: name, mime: "text/plain")
    }
    XCTAssertEqual(owned.files.map(\.name), ["first.txt", "second.txt"])
    let first = owned.files[0]
    XCTAssertEqual(
      try store.readFile(owned.id, file: first.id, owner: owned.owner!, offset: 0, count: 64),
      Data("original".utf8))
    XCTAssertThrowsError(
      try store.readFile(owned.id, file: first.id, owner: owned.owner!, offset: 0, count: 100000))
    let url = try store.verifiedFile(owned.id, file: first.id, owner: owned.owner!).0
    try Data("modified".utf8).write(to: url)
    XCTAssertThrowsError(try store.verifiedFile(owned.id, file: first.id, owner: owned.owner!))
  }

  func testThumbnailHasNoAttachmentOperationAndMissingCopyIsVisible() throws {
    let png = Data(
      base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
    )!
    var owned = try store.claim(store.create(text: "https://example.org").id)
    owned = try store.importThumbnail(owned.id, owner: owned.owner!, data: png, mime: "image/png")
    let file = try XCTUnwrap(owned.files.first)
    XCTAssertTrue(file.thumbnail)
    XCTAssertNil(file.operation)
    XCTAssertEqual(owned.bytes, png.count + owned.text.utf8.count)
    let url = try store.verifiedFile(owned.id, file: file.id, owner: owned.owner!).0
    try FileManager.default.removeItem(at: url)
    XCTAssertThrowsError(try store.verifiedFile(owned.id, file: file.id, owner: owned.owner!))
    XCTAssertEqual(try store.list().first?.files.count, 1)
    XCTAssertThrowsError(
      try store.importThumbnail(
        owned.id, owner: owned.owner!, data: Data("not an image".utf8), mime: "image/png"))
  }

  func testKilledImportReportsFailureAndReclaimsReservation() throws {
    let owned = try store.claim(store.create(text: "draft").id)
    let directory = root.appendingPathComponent(owned.id)
    try Data("1024".utf8).write(to: directory.appendingPathComponent("unfinished.part.reservation"))
    try Data("partial".utf8).write(to: directory.appendingPathComponent("unfinished.part"))
    XCTAssertEqual(try store.list().first?.importFailures, [])
    time += ShareLimits.lease + 1
    XCTAssertEqual(try store.list().first?.importFailures, ["incomplete_import"])
    XCTAssertFalse(
      FileManager.default.fileExists(
        atPath: directory.appendingPathComponent("unfinished.part").path))
  }

  func testAccountRemovalFencesOwnedDraft() throws {
    var owned = try store.claim(store.create(text: "draft").id)
    let account = UUID().uuidString
    owned.accountId = account
    owned = try store.update(owned)
    try store.clearAccount(account)
    XCTAssertThrowsError(try store.update(owned))
    XCTAssertTrue(try store.list().isEmpty)
  }

  func testConfirmedCaptionAndDestinationStayBoundToFirstFile() throws {
    let source = root.appendingPathComponent("provider.txt")
    try Data("bytes".utf8).write(to: source)
    var owned = try store.claim(store.create(text: "caption").id)
    owned.accountId = UUID().uuidString
    owned.roomId = "!room:hs"
    owned = try store.update(owned)
    owned = try store.importFile(
      owned.id, owner: owned.owner!, source: source, name: "first.txt", mime: "text/plain")
    owned = try store.importFile(
      owned.id, owner: owned.owner!, source: source, name: "second.txt", mime: "text/plain")
    for state in [DeliveryState.uploading, .uploaded, .dispatching, .sent] {
      owned.files[0].operation?.state = state
      if state == .sent { owned.files[0].operation?.eventId = "$confirmed" }
      owned = try store.update(owned)
    }
    var changed = owned
    changed.text = "different caption"
    XCTAssertThrowsError(try store.update(changed))
    changed = owned
    changed.roomId = "!other:hs"
    XCTAssertThrowsError(try store.update(changed))
    owned.files[1].operation?.state = .uploading
    XCTAssertNoThrow(try store.update(owned))
  }
  func testUnreadablePublishedManifestIsRetainedUntilExplicitDiscard() throws {
    let manifest = try store.create(text: "saved draft")
    let path = root.appendingPathComponent(manifest.id).appendingPathComponent("manifest.json")
    try Data("not valid JSON".utf8).write(to: path)
    let listed = try XCTUnwrap(store.list().first)
    XCTAssertEqual(listed.importFailures, ["unreadable_manifest"])
    XCTAssertEqual(listed.operation.state, .unknown)
    XCTAssertTrue(FileManager.default.fileExists(atPath: path.path))
    XCTAssertThrowsError(try store.discard(manifest.id, completed: true))
    try store.discard(manifest.id)
    XCTAssertTrue(try store.list().isEmpty)
  }
  func testRemoteProviderURLIsNeverInterpretedAsLocalFile() throws {
    let source = root.appendingPathComponent("provider.txt")
    try Data("local fixture".utf8).write(to: source)
    let owned = try store.claim(store.create(text: "draft").id)
    let remote = URL(string: "https://example.org" + source.path)!
    XCTAssertThrowsError(
      try store.importFile(
        owned.id, owner: owned.owner!, source: remote, name: "file", mime: "text/plain"))
    XCTAssertTrue(try store.list().first!.files.isEmpty)
  }
}
