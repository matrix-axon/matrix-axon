import Combine
import Foundation
import UIKit
import XCTest

@testable import AxonIncomingShare

private final class ComposerRoomsProtocol: URLProtocol {
  final class Reply {
    let started: XCTestExpectation
    let body: Data?
    init(started: XCTestExpectation, body: Data?) {
      self.started = started
      self.body = body
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
    Self.lock.unlock()
    guard let reply else { return }
    reply.started.fulfill()
    guard let body = reply.body else { return }
    client?.urlProtocol(
      self,
      didReceive: HTTPURLResponse(
        url: request.url!, statusCode: 200, httpVersion: "HTTP/1.1",
        headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
    client?.urlProtocol(self, didLoad: body)
    client?.urlProtocolDidFinishLoading(self)
  }
  override func stopLoading() {}
}

final class ComposerLifecycleTests: XCTestCase {
  func testCancelInterruptsBlockedRoomRequestAndRemovesItsDraft() throws {
    try checkCancellation(stalled: true)
  }
  func testExternallyDiscardedComposerCannotSendOrRecreateTheDraft() throws {
    try checkCancellation(stalled: false)
  }
  func testSevenCancelledComposersLeaveNoClaims() throws {
    for _ in 0..<7 { try checkCancellation(stalled: true) }
  }
  private func checkCancellation(stalled: Bool) throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let store = try ShareStore(root: root)
    defer { try? FileManager.default.removeItem(at: root) }
    let host = UUID().uuidString.lowercased() + ".composer-fixture.invalid"
    let session = ShareSession(
      installation: "fixture", server: "https://" + host, credentialAvailable: true, expiresAt: nil)
    try store.publish(session, credential: {})
    let started = expectation(description: "rooms request started")
    let closed = expectation(description: "composer closed promptly")
    let settled = expectation(description: "cleanup finished")
    let loaded = expectation(description: "composer loaded")
    let body = try JSONSerialization.data(withJSONObject: [
      "data": [
        [
          "account_id": UUID().uuidString, "account_user_id": "@fixture:example.org",
          "room_id": "!fixture:example.org", "name": "Fixture",
        ]
      ]
    ])
    ComposerRoomsProtocol.lock.lock()
    ComposerRoomsProtocol.replies[host] = .init(started: started, body: stalled ? nil : body)
    ComposerRoomsProtocol.lock.unlock()
    defer {
      ComposerRoomsProtocol.lock.lock()
      ComposerRoomsProtocol.replies.removeValue(forKey: host)
      ComposerRoomsProtocol.lock.unlock()
    }
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [ComposerRoomsProtocol.self]
    let http = ShareHTTP(configuration: config)
    defer { http.close() }
    let model = ShareComposer(
      transport: http,
      prepare: {
        ShareComposer.Session(store: store, credential: { (session, "synthetic-token") })
      },
      close: {
        closed.fulfill()
        settled.fulfill()
      })
    let subscription = model.$busy.dropFirst().filter { !$0 }.prefix(1).sink { _ in loaded.fulfill()
    }
    defer { subscription.cancel() }
    let item = NSExtensionItem()
    item.attachments = [NSItemProvider(object: URL(string: "https://example.org/")! as NSURL)]
    model.start([item])
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
