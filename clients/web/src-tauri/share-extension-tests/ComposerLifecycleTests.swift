import Combine
import Foundation
import UIKit
import XCTest

@testable import AxonIncomingShare

private final class ComposerRoomsProtocol: URLProtocol {
  final class Reply {
    let started: XCTestExpectation
    var body: Data?
    var accounts: Data
    var response: ((URLRequest) -> (Int, Data))?
    init(started: XCTestExpectation, body: Data?, accounts: Data) {
      self.started = started
      started.assertForOverFulfill = false
      self.body = body
      self.accounts = accounts
    }
  }
  static let lock = NSLock()
  static var replies: [String: Reply] = [:]
  override class func canInit(with request: URLRequest) -> Bool {
    request.url?.host?.hasSuffix(".composer-fixture.invalid") == true
  }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    Self.lock.lock()
    let reply = Self.replies[request.url!.host!]
    let body = request.url?.path == "/v1/accounts" ? reply?.accounts : reply?.body
    let responder = reply?.response
    Self.lock.unlock()
    guard let reply else { return }
    let path = request.url?.path
    if path == "/v1/rooms" { reply.started.fulfill() }
    let status: Int
    let payload: Data
    if path != "/v1/rooms" && path != "/v1/accounts", let responder {
      (status, payload) = responder(request)
    } else {
      guard let body else { return }
      (status, payload) = (200, body)
    }
    client?.urlProtocol(
      self,
      didReceive: HTTPURLResponse(
        url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
        headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
    client?.urlProtocol(self, didLoad: payload)
    client?.urlProtocolDidFinishLoading(self)
  }
  override func stopLoading() {}
}

final class ComposerLifecycleTests: XCTestCase {
  func testTextOnlyRejectionExplainsCodeAndUnknownOutcomeUsesSingularMessage() throws {
    for status in [400, 500] {
      let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
      fixture.respond { _ in (status, Data("{\"error\":{\"code\":\"bad_request\"}}".utf8)) }
      let model = fixture.composer()
      load(model) { model.start([fixture.item]) }
      model.chooseRoom("!first:example.org")
      load(model) { model.send() }
      XCTAssertTrue(model.status.contains(status == 400 ? "Not sent: bad_request" : "Delivery outcome unknown"))
      XCTAssertFalse(model.status.contains("another item"))
      if status == 400 { XCTAssertTrue(model.status.contains("Edit the text")) }
      model.cancel()
      wait(for: [fixture.closed], timeout: 3)
    }
    XCTAssertTrue(ShareComposer.importProblem("too_many_source_previews").contains("link previews"))
    XCTAssertFalse(ShareComposer.importProblem("unknown_future_category").contains("image"))
  }

  func testConfirmedTextSinglePhotoAndBatchCloseAutomaticallyAfterCleanup() throws {
    for count in 0...2 {
      let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
      var sent = 0
      fixture.respond { request in
        if request.url!.path.hasSuffix("/media/uploads") {
          return (200, Data("{\"data\":{\"upload_id\":\"\(UUID().uuidString)\"}}".utf8))
        }
        sent += 1
        return (200, Data("{\"data\":{\"event_id\":\"$sent-\(sent)\"}}".utf8))
      }
      var closed = 0
      let model = fixture.composer { closed += 1 }
      let item = count == 0 ? fixture.item : fixture.photos()
      if count == 1 { item.attachments = Array(item.attachments!.prefix(1)) }
      load(model) { model.start([item]) }
      model.chooseRoom("!first:example.org")
      XCTAssertTrue(model.canSend)
      model.send()
      wait(for: [fixture.closed], timeout: 5)
      XCTAssertEqual(sent, max(1, count))
      XCTAssertEqual(closed, 1)
      XCTAssertTrue(try fixture.store.list().isEmpty)
      model.send()
      model.cancel()
      XCTAssertEqual(closed, 1)
      XCTAssertEqual(sent, max(1, count))
    }
  }

  func testPartialFailureAndUnknownDeliveryStayOpenWithConfirmedOutcomes() throws {
    for status in [400, 500] {
      let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
      var sent = 0
      fixture.respond { request in
        if request.url!.path.hasSuffix("/media/uploads") {
          return (200, Data("{\"data\":{\"upload_id\":\"\(UUID().uuidString)\"}}".utf8))
        }
        sent += 1
        if sent == 1 { return (200, Data("{\"data\":{\"event_id\":\"$confirmed\"}}".utf8)) }
        return (status, Data("{\"error\":{\"code\":\"bad_request\"}}".utf8))
      }
      var closed = 0
      let model = fixture.composer { closed += 1 }
      load(model) { model.start([fixture.photos()]) }
      model.chooseRoom("!first:example.org")
      load(model) { model.send() }
      XCTAssertEqual(closed, 0)
      XCTAssertEqual(model.record?.files.map { $0.operation?.state }, [.sent, status == 400 ? .failed : .unknown])
      XCTAssertEqual(model.record?.files.first?.operation?.eventId, "$confirmed")
      XCTAssertFalse(model.canSend)
      model.cancel()
      wait(for: [fixture.closed], timeout: 3)
      XCTAssertEqual(try fixture.store.list().first?.files.first?.operation?.eventId, "$confirmed")
    }
  }

  func testCleanupFailureRetainsConfirmedSuccessAndDoneRetriesWithoutSending() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    var sent = 0
    fixture.respond { _ in
      sent += 1
      return (200, Data("{\"data\":{\"event_id\":\"$confirmed\"}}".utf8))
    }
    var closed = 0
    let model = fixture.composer { closed += 1 }
    load(model) { model.start([fixture.item]) }
    model.chooseRoom("!first:example.org")
    let blocked = model.$record.filter { $0?.completed == true }.prefix(1).sink { _ in
      do { try fixture.blockStorage() } catch { XCTFail("Could not inject cleanup failure: \(error)") }
    }
    let failed = expectation(description: "completed cleanup failure shown")
    let status = model.$status.filter { $0.contains("completed draft could not be removed") }.prefix(1).sink { _ in failed.fulfill() }
    model.send()
    wait(for: [failed], timeout: 5)
    blocked.cancel()
    status.cancel()
    XCTAssertEqual(closed, 0)
    XCTAssertEqual(sent, 1)
    XCTAssertFalse(model.canSend)
    XCTAssertFalse(model.status.contains("Nothing was sent"))
    try fixture.restoreStorage()
    XCTAssertEqual(try fixture.store.list().first?.operation.state, .sent)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    XCTAssertEqual(sent, 1)
    XCTAssertTrue(try fixture.store.list().isEmpty)
  }

  func testPhotoOnlyComposerAllowsEmptyCaptionRemovalAndDraftRecovery() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let image = UIGraphicsImageRenderer(size: CGSize(width: 32, height: 24)).image { context in
      UIColor.systemBlue.setFill()
      context.fill(CGRect(x: 0, y: 0, width: 32, height: 24))
    }
    let item = NSExtensionItem()
    item.attachments = [NSItemProvider(object: image), NSItemProvider(object: image)]
    let model = fixture.composer()
    load(model) { model.start([item]) }
    XCTAssertEqual(model.record?.files.filter { !$0.thumbnail }.count, 2)
    XCTAssertEqual(model.images.count, 2)
    XCTAssertEqual(model.text, "")
    XCTAssertNil(model.validation)
    model.chooseRoom("!first:example.org")
    XCTAssertTrue(model.canSend)
    let first = try XCTUnwrap(model.record?.files.first?.id)
    load(model) { model.removePhoto(first) }
    XCTAssertEqual(model.record?.files.count, 1)
    XCTAssertEqual(model.images.count, 1)
    model.setPhotoCaption("Keep this first-photo caption", file: model.record!.files[0].id)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    let saved = try XCTUnwrap(fixture.store.list().first)
    XCTAssertEqual(saved.files.count, 1)
    XCTAssertEqual(saved.files[0].caption, "Keep this first-photo caption")
    XCTAssertEqual(saved.files[0].operation?.state, .ready)
    XCTAssertNil(saved.owner)
  }

  func testPhotoSizeChoiceAppliesToAllAndSupportsIndividualOverrides() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    // Fix the pixel count across simulator display scales; 3x would create two 19 MP images.
    let format = UIGraphicsImageRendererFormat()
    format.scale = 1
    format.opaque = true
    let image = UIGraphicsImageRenderer(
      size: CGSize(width: 1800, height: 1200), format: format
    ).image { context in
      UIColor.systemBlue.setFill()
      context.fill(CGRect(x: 0, y: 0, width: 1800, height: 1200))
    }
    XCTAssertEqual(image.cgImage?.width, 1800)
    XCTAssertEqual(image.cgImage?.height, 1200)
    let item = NSExtensionItem()
    item.attachments = [NSItemProvider(object: image), NSItemProvider(object: image)]
    let model = fixture.composer()
    // Initial import includes provider encoding and staging of both photos on CI.
    load(model, timeout: 15) { model.start([item]) }
    XCTAssertEqual(model.commonPhotoSize, .original)
    let first = try XCTUnwrap(model.record?.files.first?.id)
    model.text = "Caption survives resizing"
    load(model) { model.setPhotoSize(.small) }
    XCTAssertEqual(model.commonPhotoSize, .small)
    XCTAssertEqual(model.record?.text, "Caption survives resizing")
    XCTAssertTrue(
      try XCTUnwrap(model.record?.files).allSatisfy {
        $0.photoSize == .small && $0.photoBytes != nil
      })
    load(model) { model.setPhotoSize(.medium, file: first) }
    XCTAssertNil(model.commonPhotoSize)
    XCTAssertEqual(model.record?.files[0].photoSize, .medium)
    XCTAssertEqual(model.record?.files[1].photoSize, .small)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    let recovered = try XCTUnwrap(fixture.store.list().first)
    XCTAssertEqual(recovered.files.map(\.photoSize), [.medium, .small])
  }

  func testRenamingSavesPendingCaptionsAndSurvivesImmediateClose() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    let files = try XCTUnwrap(model.record?.files)
    model.text = "Separate source"
    model.setPhotoCaption("Keep my caption", file: files[0].id)
    model.setPhotoName("Café coast.heic", file: files[0].id)
    XCTAssertEqual(model.photoName(files[0].id), "Café coast.heic")
    XCTAssertEqual(model.record?.files[0].id, files[0].id)
    XCTAssertEqual(model.record?.files[1], files[1])
    XCTAssertEqual(model.photoCaption(files[0].id), "Keep my caption")
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    let recovered = try XCTUnwrap(fixture.store.list().first)
    XCTAssertEqual(recovered.files[0].name, "Café coast.heic")
    XCTAssertEqual(recovered.files[0].caption, "Keep my caption")
    XCTAssertEqual(recovered.text, "Separate source")
  }

  func testOverLimitPhotoSelectionStagesNothingAndClearlyBlocksSend() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    let item = fixture.photos()
    item.attachments = Array(repeating: item.attachments![0], count: 11)
    load(model) { model.start([item]) }
    XCTAssertTrue(model.record?.files.isEmpty == true)
    XCTAssertTrue(model.record?.importFailures.contains("too_many_photos") == true)
    XCTAssertTrue(ShareComposer.importProblem("too_many_photos").contains("10 photos"))
    model.chooseRoom("!first:example.org")
    XCTAssertFalse(model.canSend)
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testEmptyPhotoShareUsesPlainEmptyStateAndKeepsSendDisabled() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    let item = fixture.photos()
    item.attachments = Array(item.attachments!.prefix(1))
    load(model) { model.start([item]) }
    model.chooseRoom("!first:example.org")
    XCTAssertTrue(model.canSend)
    let file = try XCTUnwrap(model.record?.files.first)
    load(model) { model.removePhoto(file.id) }
    XCTAssertEqual(model.validationMessage, "Nothing to send.")
    XCTAssertTrue(model.status.isEmpty)
    XCTAssertFalse(model.canSend)
    XCTAssertTrue(model.record?.files.isEmpty == true)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
  }

  func testInlineFilenameIsAppliedBeforeImmediateSend() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    var uploadedNames: [String] = []
    fixture.respond { request in
      if request.url!.path.hasSuffix("/media/uploads") {
        uploadedNames.append(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!.first { $0.name == "filename" }!.value!)
        return (200, Data("{\"data\":{\"upload_id\":\"\(UUID().uuidString)\"}}".utf8))
      }
      return (200, Data("{\"data\":{\"event_id\":\"$sent\"}}".utf8))
    }
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    model.chooseRoom("!first:example.org")
    let files = try XCTUnwrap(model.record?.files)
    model.setPhotoName("Coast trip.heic", file: files[0].id)
    model.send()
    wait(for: [fixture.closed], timeout: 5)
    XCTAssertEqual(uploadedNames, ["Coast trip.heic", files[1].name])
  }

  func testTemporarilyEmptyFilenameRetainsLastValidNameUntilCorrected() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    let file = try XCTUnwrap(model.record?.files.first)
    model.setPhotoName("", file: file.id)
    let saved = expectation(description: "filename autosave completed")
    let subscription = model.$record.dropFirst().prefix(1).sink { _ in saved.fulfill() }
    model.save()
    wait(for: [saved], timeout: 5)
    subscription.cancel()
    XCTAssertEqual(model.record?.files[0].name, file.name)
    XCTAssertEqual(model.photoName(file.id), "")
    XCTAssertEqual(model.validationMessage, "Enter a filename for each photo.")
    model.setPhotoName("Corrected coast.heic", file: file.id)
    load(model) { model.setPhotoSize(.small) }
    XCTAssertEqual(model.record?.files[0].name, "Corrected coast.heic")
    XCTAssertNil(model.validationMessage)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
  }

  func testSendStaysDisabledWhilePhotoUploadIsInFlight() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let uploadStarted = expectation(description: "upload started")
    let release = DispatchSemaphore(value: 0)
    defer { release.signal() }
    fixture.respond { request in
      if request.url!.path.hasSuffix("/media/uploads") {
        uploadStarted.fulfill()
        _ = release.wait(timeout: .now() + 4)
      }
      return (400, Data("{\"error\":{\"code\":\"bad_request\"}}".utf8))
    }
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    model.chooseRoom("!first:example.org")
    XCTAssertTrue(model.canSend)
    model.send()
    wait(for: [uploadStarted], timeout: 3)
    XCTAssertTrue(model.busy)
    XCTAssertFalse(model.canSend)
    XCTAssertEqual(model.status, "Sending…")
    load(model) { release.signal() }
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testMasterCaptionAndReorderKeepSourceAndPerPhotoOverrides() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    let files = try XCTUnwrap(model.record?.files)
    model.text = "Separate source"
    model.setMasterCaption("Together 🌊")
    XCTAssertEqual(model.masterCaption, "Together 🌊")
    XCTAssertEqual(model.photoCaption(files[1].id), "Together 🌊")
    model.setPhotoCaption("Individual", file: files[1].id)
    model.setPhotoName("First coast.heic", file: files[0].id)
    XCTAssertEqual(model.masterCaption, "")
    load(model) { model.movePhotos(from: IndexSet(integer: 0), to: 2) }
    XCTAssertEqual(model.record?.files.map(\.id), [files[1].id, files[0].id])
    XCTAssertEqual(model.record?.files.map(\.caption), ["Individual", "Together 🌊"])
    XCTAssertEqual(model.text, "Separate source")
    XCTAssertEqual(model.record?.files[1].name, "First coast.heic")
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    let recovered = try XCTUnwrap(fixture.store.list().first)
    XCTAssertEqual(recovered.files.map(\.id), [files[1].id, files[0].id])
    XCTAssertEqual(recovered.files.map(\.caption), ["Individual", "Together 🌊"])
    XCTAssertEqual(recovered.files[1].name, "First coast.heic")
  }

  func testPhotoCaptionsSaveWithTheirPhotoAndSurviveRemovalAndImmediateClose() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    let files = try XCTUnwrap(model.record?.files)
    model.setPhotoCaption("First beach", file: files[0].id)
    model.setPhotoCaption("Second café 🌊\nTwo lines", file: files[1].id)
    load(model) { model.setPhotoSize(.small) }
    XCTAssertEqual(model.record?.version, 4)
    XCTAssertEqual(model.record?.files[0].caption, "First beach")
    XCTAssertEqual(model.record?.files[1].caption, "Second café 🌊\nTwo lines")
    load(model) { model.removePhoto(files[0].id) }
    XCTAssertEqual(model.photoCaption(files[1].id), "Second café 🌊\nTwo lines")
    model.setPhotoCaption("Edited remaining caption", file: files[1].id)
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    let recovered = try XCTUnwrap(fixture.store.list().first)
    XCTAssertEqual(recovered.files.map(\.id), [files[1].id])
    XCTAssertEqual(recovered.files[0].caption, "Edited remaining caption")
    XCTAssertNil(recovered.owner)
  }

  func testPreparationFailureAfterPartialDeliveryKeepsTheConfirmedOutcomeVisible() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.photos()]) }
    var saved = try XCTUnwrap(model.record)
    saved.files[0].operation!.state = .dispatching
    saved = try fixture.store.update(saved)
    saved.files[0].operation!.state = .sent
    saved.files[0].operation!.eventId = "$confirmed"
    _ = try fixture.store.update(saved)
    load(model) { model.run { throw ShareFailure.quota } }
    XCTAssertTrue(model.status.contains("Confirmed items: 1"), model.status)
    XCTAssertFalse(model.status.contains("Nothing was sent"), model.status)
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testFailedFinishKeepsHeartbeatAndAllowsRetry() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer(heartbeatInterval: 0.05)
    load(model) { model.start([fixture.item]) }
    try fixture.blockStorage()
    load(model) { model.finish(discard: false) }
    XCTAssertTrue(model.editable)
    let revision = try XCTUnwrap(model.record?.revision)
    try fixture.restoreStorage()
    let renewed = expectation(description: "heartbeat survives failed finish")
    let subscription = model.$record.filter { ($0?.revision ?? 0) > revision }.prefix(1).sink { _ in
      renewed.fulfill()
    }
    wait(for: [renewed], timeout: 1)
    subscription.cancel()
    model.finish(discard: false)
    wait(for: [fixture.closed], timeout: 3)
    XCTAssertNil(try fixture.store.list().first?.owner)
  }

  func testTransientHeartbeatFailurePreservesDeliveryStatus() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer(heartbeatInterval: 0.05)
    load(model) { model.start([fixture.item]) }
    let status = "Delivery outcome unknown. Check the destination before resending."
    model.status = status
    try fixture.blockStorage()
    let checked = expectation(description: "heartbeat attempted while storage unavailable")
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
      XCTAssertEqual(model.status, status)
      checked.fulfill()
    }
    wait(for: [checked], timeout: 1)
    try fixture.restoreStorage()
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testEditDebounceDuringBlockingWorkIsRetriedWhenItSettles() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms"))
    let model = fixture.composer()
    load(model) { model.start([fixture.item]) }
    let gate = DispatchSemaphore(value: 0)
    let saved = expectation(description: "edit persisted after blocking work")
    let edited = "Edited while an action was busy"
    let subscription = model.$record.filter { $0?.text == edited }.prefix(1).sink { _ in
      saved.fulfill()
    }
    model.run {
      guard gate.wait(timeout: .now() + 3) == .success else { throw ShareFailure.expired }
      return ""
    }
    model.text = edited
    model.edited()
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { gate.signal() }
    wait(for: [saved], timeout: 2)
    subscription.cancel()
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }
  func testCancelInterruptsBlockedRoomRequestAndRemovesItsDraft() throws {
    try checkCancellation(stalled: true)
  }
  func testExternallyDiscardedComposerCannotSendOrRecreateTheDraft() throws {
    try checkCancellation(stalled: false)
  }
  func testSevenCancelledComposersLeaveNoClaims() throws {
    for _ in 0..<7 { try checkCancellation(stalled: true) }
  }
  func testSoleAccountIsSelectedAndPersistedBeforeChoosingRoom() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms loaded"))
    let model = fixture.composer()
    load(model) { model.start([fixture.item]) }
    XCTAssertEqual(model.accounts.count, 1)
    XCTAssertEqual(model.account, fixture.first)
    XCTAssertEqual(model.record?.accountId, fixture.first)
    XCTAssertEqual(model.accountTitle, "@first:example.org")
    XCTAssertEqual(model.room, "", "A first share must still require an explicit room choice")
    XCTAssertFalse(model.canSend)
    model.chooseRoom("!first:example.org")
    XCTAssertTrue(model.canSend)
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testNoAccountsExplainsRecoveryAndReloadAutomaticallySelectsNewAccount() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms loaded"))
    try fixture.reply(accounts: [], rooms: [])
    let model = fixture.composer()
    load(model) { model.start([fixture.item]) }
    XCTAssertTrue(model.status.contains("No active Matrix accounts"), model.status)
    XCTAssertTrue(model.status.contains("Open Axon"))
    XCTAssertTrue(model.status.contains("reload"))
    XCTAssertEqual(model.text, "https://example.org/")
    XCTAssertNotNil(model.record)
    XCTAssertEqual(model.account, "")
    XCTAssertFalse(model.canSend)
    try fixture.reply(accounts: [fixture.account(fixture.first)], rooms: fixture.rooms)
    load(model) { model.reloadDestinations() }
    XCTAssertEqual(model.account, fixture.first)
    XCTAssertEqual(model.record?.accountId, fixture.first)
    XCTAssertEqual(model.status, "")
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testOneAccountWithoutJoinedRoomsIsNotReportedAsNoAccounts() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms loaded"))
    try fixture.reply(accounts: [fixture.account(fixture.first)], rooms: [])
    let model = fixture.composer()
    load(model) { model.start([fixture.item]) }
    XCTAssertEqual(model.accounts.count, 1)
    XCTAssertEqual(model.account, fixture.first)
    XCTAssertEqual(model.accountTitle, "@first:example.org")
    XCTAssertTrue(model.status.contains("No joined rooms"), model.status)
    XCTAssertFalse(model.canSend)
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testMultipleAccountsIncludeRoomlessAccountAndReloadClearsStaleDestination() throws {
    let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms loaded"))
    let second = UUID().uuidString
    try fixture.reply(
      accounts: [fixture.account(fixture.first), fixture.account(second)], rooms: fixture.rooms)
    let model = fixture.composer()
    load(model) { model.start([fixture.item]) }
    XCTAssertEqual(model.accounts.count, 2, "An account without rooms still counts")
    XCTAssertEqual(model.account, "", "Multiple accounts still require an account choice")
    model.chooseAccount(fixture.first)
    model.chooseRoom("!first:example.org")
    XCTAssertTrue(model.canSend)
    try fixture.reply(accounts: [fixture.account(second)], rooms: [])
    load(model) { model.reloadDestinations() }
    XCTAssertEqual(model.account, second)
    XCTAssertEqual(model.record?.accountId, second)
    XCTAssertEqual(model.room, "")
    XCTAssertNil(model.record?.roomId)
    XCTAssertFalse(model.canSend)
    try fixture.reply(accounts: [], rooms: [])
    load(model) { model.reloadDestinations() }
    XCTAssertEqual(model.account, "")
    XCTAssertNil(model.record?.accountId)
    XCTAssertTrue(model.status.contains("No active Matrix accounts"), model.status)
    model.cancel()
    wait(for: [fixture.closed], timeout: 3)
  }

  func testRememberedRoomStillRestoresForOneOrMultipleAccounts() throws {
    for multiple in [false, true] {
      let fixture = try ComposerFixture(started: XCTestExpectation(description: "rooms loaded"))
      var accounts = [fixture.account(fixture.first)]
      if multiple { accounts.append(fixture.account(UUID().uuidString)) }
      try fixture.reply(accounts: accounts, rooms: fixture.rooms)
      let first = fixture.composer()
      load(first) { first.start([fixture.item]) }
      if multiple { first.chooseAccount(fixture.first) }
      first.chooseRoom("!first:example.org")
      load(first) { first.reloadDestinations() }  // Flush and verify the persisted selection.
      first.cancel()
      wait(for: [fixture.closed], timeout: 3)
      let closed = expectation(description: "second composer closed")
      let next = fixture.composer(transport: ComposerFixture.transport()) { closed.fulfill() }
      load(next) { next.start([fixture.item]) }
      XCTAssertEqual(next.account, fixture.first)
      XCTAssertEqual(next.room, "!first:example.org")
      XCTAssertTrue(next.canSend)
      next.cancel()
      wait(for: [closed], timeout: 3)
    }
  }

  private func load(_ model: ShareComposer, timeout: TimeInterval = 5, action: () -> Void) {
    let loaded = expectation(description: "destinations loaded")
    let subscription = model.$busy.dropFirst().filter { !$0 }.prefix(1).sink { _ in loaded.fulfill()
    }
    action()
    wait(for: [loaded], timeout: timeout)
    subscription.cancel()
  }

  private func checkCancellation(stalled: Bool) throws {
    let started = expectation(description: "rooms request started")
    let closed = expectation(description: "composer closed promptly")
    let settled = expectation(description: "cleanup finished")
    let loaded = expectation(description: "composer loaded")
    let fixture = try ComposerFixture(started: started, stalled: stalled)
    let store = fixture.store
    let http = fixture.http
    let model = fixture.composer {
      closed.fulfill()
      settled.fulfill()
    }
    let subscription = model.$busy.dropFirst().filter { !$0 }.prefix(1).sink { _ in loaded.fulfill()
    }
    defer { subscription.cancel() }
    model.start([fixture.item])
    wait(for: [started], timeout: 5)
    if !stalled { wait(for: [loaded], timeout: 5) }
    XCTAssertEqual(try store.list().count, 1)
    if !stalled {
      let destination = try XCTUnwrap(model.rooms.first)
      let selected = expectation(description: "destination saved")
      let selectedSubscription = model.$record.filter {
        $0?.accountId == destination.account_id && $0?.roomId == destination.room_id
      }.prefix(1).sink { _ in selected.fulfill() }
      model.chooseAccount(destination.account_id)
      model.chooseRoom(destination.room_id)
      wait(for: [selected], timeout: 3)
      selectedSubscription.cancel()
      XCTAssertTrue(model.canSend)
      let id = try XCTUnwrap(model.record?.id)
      try store.discard(id)
      let fenced = expectation(description: "discard fences the old composer")
      let fencedSubscription = model.$record.filter { $0 != nil && $0?.owner == nil }
        .prefix(1).sink { _ in fenced.fulfill() }
      model.text += " Edited after external discard"
      model.save()
      wait(for: [fenced], timeout: 3)
      fencedSubscription.cancel()
      XCTAssertFalse(model.canSend)
      model.send()  // Must not make another network request or recreate the draft.
    }
    model.cancel()
    let result = XCTWaiter.wait(for: [closed], timeout: 3)
    XCTAssertEqual(result, .completed, "Cancel must interrupt the network request before closing")
    http.close()  // also drains the deliberately failing pre-fix test.
    wait(for: [settled], timeout: 5)
    if stalled {
      loaded.isInverted = true
      wait(for: [loaded], timeout: 0.05)
    }
    XCTAssertTrue(try store.list().isEmpty)
  }
}

private final class ComposerFixture {
  let root: URL
  let store: ShareStore
  let session: ShareSession
  let host: String
  let http: ShareHTTP
  let first = UUID().uuidString
  let closed = XCTestExpectation(description: "fixture composer closed")
  var rooms: [[String: Any]] {
    [
      [
        "account_id": first, "account_user_id": "@first:example.org",
        "room_id": "!first:example.org", "name": "First",
      ]
    ]
  }
  var item: NSExtensionItem {
    let item = NSExtensionItem()
    item.attachments = [NSItemProvider(object: URL(string: "https://example.org/")! as NSURL)]
    return item
  }
  func photos() -> NSExtensionItem {
    let image = UIGraphicsImageRenderer(size: CGSize(width: 32, height: 24)).image { context in
      UIColor.systemBlue.setFill()
      context.fill(CGRect(x: 0, y: 0, width: 32, height: 24))
    }
    let item = NSExtensionItem()
    item.attachments = [NSItemProvider(object: image), NSItemProvider(object: image)]
    return item
  }
  func account(_ id: String) -> [String: Any] {
    [
      "account_id": id, "user_id": id == first ? "@first:example.org" : "@second:example.org",
      "state": "active",
    ]
  }
  init(started: XCTestExpectation, stalled: Bool = false) throws {
    root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    store = try ShareStore(root: root)
    host = UUID().uuidString.lowercased() + ".composer-fixture.invalid"
    session = ShareSession(
      installation: "fixture", server: "https://" + host, credentialAvailable: true, expiresAt: nil)
    try store.publish(session, credential: {})
    http = Self.transport()
    let body = try JSONSerialization.data(withJSONObject: ["data": rooms])
    let accounts = try JSONSerialization.data(withJSONObject: ["data": [account(first)]])
    ComposerRoomsProtocol.lock.lock()
    ComposerRoomsProtocol.replies[host] = .init(
      started: started, body: stalled ? nil : body, accounts: accounts)
    ComposerRoomsProtocol.lock.unlock()
  }
  func reply(accounts: [[String: Any]], rooms: [[String: Any]]) throws {
    let accountData = try JSONSerialization.data(withJSONObject: ["data": accounts])
    let roomData = try JSONSerialization.data(withJSONObject: ["data": rooms])
    ComposerRoomsProtocol.lock.lock()
    ComposerRoomsProtocol.replies[host]?.accounts = accountData
    ComposerRoomsProtocol.replies[host]?.body = roomData
    ComposerRoomsProtocol.lock.unlock()
  }
  func respond(_ response: @escaping (URLRequest) -> (Int, Data)) {
    ComposerRoomsProtocol.lock.lock()
    ComposerRoomsProtocol.replies[host]?.response = response
    ComposerRoomsProtocol.lock.unlock()
  }
  static func transport() -> ShareHTTP {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [ComposerRoomsProtocol.self]
    return ShareHTTP(configuration: config)
  }
  func blockStorage() throws {
    let lock = root.appendingPathComponent(".lock")
    try FileManager.default.removeItem(at: lock)
    try FileManager.default.createDirectory(at: lock, withIntermediateDirectories: false)
  }
  func restoreStorage() throws {
    try FileManager.default.removeItem(at: root.appendingPathComponent(".lock"))
  }
  func composer(
    transport: ShareHTTP? = nil, heartbeatInterval: TimeInterval = 60,
    close: @escaping () -> Void = {}
  ) -> ShareComposer {
    let store = store
    let session = session
    let closed = closed
    return ShareComposer(
      transport: transport ?? http,
      prepare: { .init(store: store, credential: { (session, "synthetic-token") }) },
      heartbeatInterval: heartbeatInterval,
      close: {
        closed.fulfill()
        close()
      })
  }
  deinit {
    http.close()
    ComposerRoomsProtocol.lock.lock()
    ComposerRoomsProtocol.replies.removeValue(forKey: host)
    ComposerRoomsProtocol.lock.unlock()
    try? FileManager.default.removeItem(at: root)
  }
}
