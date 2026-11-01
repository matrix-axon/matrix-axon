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

  func testFailedImportPublicationRemovesMovedFileAndAllowsRetry() throws {
    let fm = FileManager.default
    let source = root.appendingPathComponent("provider.txt")
    try Data("provider bytes".utf8).write(to: source)
    let item = try store.claim(store.create(text: "caption").id)
    let directory = root.appendingPathComponent(item.id)
    let metadata = directory.appendingPathComponent("manifest.json")
    let original = try Data(contentsOf: metadata)
    var checks = 0
    var fault: Error?
    // The second owner check occurs after loading metadata, immediately before
    // the copied file is moved and its manifest is published.
    let failing = try ShareStore(
      root: root,
      now: {
        checks += 1
        if checks == 2 {
          do {
            try fm.removeItem(at: metadata)
            try fm.createDirectory(at: metadata, withIntermediateDirectories: false)
          } catch { fault = error }
        }
        return self.time
      })
    XCTAssertThrowsError(
      try failing.importFile(
        item.id, owner: item.owner!, source: source, name: "file.txt", mime: "text/plain"))
    XCTAssertNil(fault)
    XCTAssertEqual(checks, 2)
    XCTAssertEqual(try fm.contentsOfDirectory(atPath: directory.path), ["manifest.json"])
    try fm.removeItem(at: metadata)
    try original.write(to: metadata)
    XCTAssertTrue(try store.list().first!.files.isEmpty)
    let retried = try store.importFile(
      item.id, owner: item.owner!, source: source, name: "file.txt", mime: "text/plain")
    XCTAssertEqual(retried.files.count, 1)
    XCTAssertEqual(
      try Data(contentsOf: directory.appendingPathComponent(retried.files[0].id)),
      Data("provider bytes".utf8))
  }

  func testCorruptCleanupJournalRepairsDurableResetBeforeRetry() throws {
    let control = root.appendingPathComponent("../" + UUID().uuidString).standardizedFileURL
    defer { try? FileManager.default.removeItem(at: control) }
    let path = control.appendingPathComponent("cleanup.json")
    let journal = try ShareStore(root: control)
    let corrupt = [
      Data("{\"reset\":".utf8), Data("garbage".utf8),
      try ShareJSON.encode(CleanupFixture(reset: false, accounts: ["not-a-uuid"])),
      try ShareJSON.encode(
        CleanupFixture(reset: false, accounts: Array(repeating: UUID().uuidString, count: 129))),
    ]
    for bytes in corrupt {
      try store.publish(
        ShareSession(
          installation: "install", server: "https://axon.example",
          credentialAvailable: true, expiresAt: nil), credential: {})
      _ = try store.create(text: "must be fenced")
      try bytes.write(to: path)
      // A busy destination must leave a repaired, durable full-reset intent.
      try store.coordinated { XCTAssertThrowsError(try journal.reconcileCleanup(into: store)) }
      let repaired = try JSONDecoder().decode(CleanupFixture.self, from: Data(contentsOf: path))
      XCTAssertTrue(repaired.reset)
      XCTAssertTrue(repaired.accounts.isEmpty)
      try ShareStore(root: control).reconcileCleanup(into: store)
      XCTAssertNil(try store.session())
      XCTAssertTrue(try store.list().isEmpty)
      XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))
    }
    try Data("truncated".utf8).write(to: path)
    try journal.recordCleanup(account: UUID().uuidString)
    let repaired = try JSONDecoder().decode(CleanupFixture.self, from: Data(contentsOf: path))
    XCTAssertTrue(repaired.reset)
    try journal.reconcileCleanup(into: store)
    XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))
  }

  private struct CleanupFixture: Codable {
    let reset: Bool
    let accounts: [String]
  }

  func testVerifiedUploadSnapshotSurvivesRetainedFileMutationAndCleansUp() throws {
    let source = root.appendingPathComponent("provider.txt")
    try Data("original".utf8).write(to: source)
    var owned = try store.claim(store.create(text: "caption").id)
    owned = try store.importFile(
      owned.id, owner: owned.owner!, source: source,
      name: "file.txt", mime: "text/plain")
    let file = owned.files[0]
    let retained = root.appendingPathComponent(owned.id).appendingPathComponent(file.id)
    var snapshot: URL!
    try store.withVerifiedFile(owned.id, file: file.id, owner: owned.owner!) { url, _ in
      snapshot = url
      // Both in-place mutation and path replacement must leave verified bytes
      // stable for the whole transport callback.
      try Data("changed!".utf8).write(to: retained)
      try Data("replaced".utf8).write(to: retained, options: .atomic)
      XCTAssertEqual(try Data(contentsOf: url), Data("original".utf8))
      let peer = try ShareStore(root: root, now: { self.time })
      XCTAssertNoThrow(try peer.coordinated {})
    }
    XCTAssertFalse(FileManager.default.fileExists(atPath: snapshot.path))
    XCTAssertThrowsError(
      try store.withVerifiedFile(
        owned.id, file: file.id,
        owner: owned.owner!
      ) { _, _ in })
    XCTAssertFalse(
      try FileManager.default.contentsOfDirectory(atPath: retained.deletingLastPathComponent().path)
        .contains(where: { $0.hasSuffix(".part") }))
  }

  func testUnavailableMetadataCanBeDiscardedWithoutFollowingItsSymlink() throws {
    let owned = try store.create(text: "draft")
    let metadata = root.appendingPathComponent(owned.id).appendingPathComponent("manifest.json")
    let target = root.appendingPathComponent("unrelated.json")
    try Data("keep this file".utf8).write(to: target)
    try FileManager.default.removeItem(at: metadata)
    try FileManager.default.createSymbolicLink(at: metadata, withDestinationURL: target)
    XCTAssertEqual(try store.list().first?.importFailures, ["unreadable_manifest"])
    XCTAssertThrowsError(try store.discard(owned.id, completed: true))
    try store.discard(owned.id)
    XCTAssertTrue(try store.list().isEmpty)
    XCTAssertEqual(try Data(contentsOf: target), Data("keep this file".utf8))
  }

  func testNonGrowingUpdatesDoNotScanOtherRecordsForQuota() throws {
    var owned = try store.claim(store.create(text: "draft").id)
    let unrelated = try store.claim(store.create(text: "other").id)
    var growing = try store.claim(store.create(text: "new").id)
    try Data("unreadable reservation".utf8).write(
      to: root.appendingPathComponent(unrelated.id)
        .appendingPathComponent("unfinished.part.reservation"))
    owned.operation.state = .dispatching
    owned = try store.update(owned)
    XCTAssertEqual(owned.operation.state, .dispatching)
    owned = try store.update(owned)
    XCTAssertNotNil(owned.owner)
    growing.text += "larger"
    XCTAssertThrowsError(try store.update(growing))
  }

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
    let url = root.appendingPathComponent(owned.id).appendingPathComponent(first.id)
    try Data("modified".utf8).write(to: url)
    XCTAssertThrowsError(
      try store.withVerifiedFile(owned.id, file: first.id, owner: owned.owner!) { _, _ in })
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
    let url = root.appendingPathComponent(owned.id).appendingPathComponent(file.id)
    try FileManager.default.removeItem(at: url)
    XCTAssertThrowsError(
      try store.withVerifiedFile(owned.id, file: file.id, owner: owned.owner!) { _, _ in })
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
