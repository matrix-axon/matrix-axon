import Foundation
import XCTest

@testable import AxonIncomingShare

private final class ReplyProtocol: URLProtocol {
  struct Reply {
    let body: Data
    let declaredBytes: Int?
  }
  private static let lock = NSLock()
  private static var replies: [String: Reply] = [:]
  static func register(_ reply: Reply, host: String) {
    lock.lock()
    defer { lock.unlock() }
    replies[host] = reply
  }
  static func remove(_ host: String) {
    lock.lock()
    defer { lock.unlock() }
    replies.removeValue(forKey: host)
  }
  override class func canInit(with request: URLRequest) -> Bool {
    request.url?.host?.hasSuffix(".axon-fixture.invalid") == true
  }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    Self.lock.lock()
    let reply = Self.replies[request.url!.host!]
    Self.lock.unlock()
    guard let reply else {
      client?.urlProtocol(self, didFailWithError: URLError(.resourceUnavailable))
      return
    }
    var headers = ["Content-Type": "application/json"]
    if let declared = reply.declaredBytes { headers["Content-Length"] = String(declared) }
    client?.urlProtocol(
      self,
      didReceive: HTTPURLResponse(
        url: request.url!, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: headers)!,
      cacheStoragePolicy: .notAllowed)
    for start in stride(from: 0, to: reply.body.count, by: 64 * 1024) {
      client?.urlProtocol(
        self, didLoad: reply.body.subdata(in: start..<min(start + 64 * 1024, reply.body.count)))
    }
    client?.urlProtocolDidFinishLoading(self)
  }
  override func stopLoading() {}
}

final class ShareHTTPTests: XCTestCase {
  private func withHTTP<T>(
    body: Data, declaredBytes: Int?, _ action: (ShareHTTP, ShareSession) throws -> T
  ) throws -> T {
    let host = UUID().uuidString.lowercased() + ".axon-fixture.invalid"
    ReplyProtocol.register(.init(body: body, declaredBytes: declaredBytes), host: host)
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [ReplyProtocol.self]
    let http = ShareHTTP(configuration: configuration)
    defer {
      http.close()
      ReplyProtocol.remove(host)
    }
    return try action(
      http,
      ShareSession(
        installation: "synthetic", server: "https://" + host, credentialAvailable: true,
        expiresAt: nil))
  }
  func testClosedHTTPRejectsNewRequestsWithoutCreatingATask() throws {
    try withHTTP(body: Data(), declaredBytes: 0) { http, session in
      http.close()
      http.close()
      XCTAssertThrowsError(try http.request(descriptor: session, token: "synthetic-token", path: "/v1/rooms")) {
        XCTAssertEqual($0 as? ShareFailure, .cancelled)
      }
    }
  }
  func testSmallResponsePassesThroughRealDelegate() throws {
    let body = Data("{\"data\":[]}".utf8)
    try withHTTP(body: body, declaredBytes: body.count) { http, session in
      let response = try http.request(
        descriptor: session, token: "synthetic-not-a-credential", path: "/v1/rooms")
      XCTAssertEqual(response.0, 200)
      XCTAssertEqual(response.1, body)
    }
  }
  func testDeclaredResponseLimitIsNotMisclassifiedAsOffline() throws {
    try withHTTP(body: Data(), declaredBytes: 3 * 1024 * 1024) { http, session in
      XCTAssertThrowsError(
        try http.request(
          descriptor: session, token: "synthetic-not-a-credential", path: "/v1/rooms",
          limit: 2 * 1024 * 1024)
      ) {
        let failure = $0 as? ShareResponseLimitFailure
        XCTAssertEqual(failure?.limit, 2 * 1024 * 1024)
        XCTAssertEqual(failure?.observedBytes, 3 * 1024 * 1024)
      }
    }
  }
  func testRoomPickerUsesSeparateBoundedBudgetThroughRealHTTPDelegate() throws {
    let account = UUID().uuidString
    let rooms: [[String: Any]] = (0..<150).map { index in
      [
        "account_id": account, "account_user_id": "@test:example.org",
        "room_id": "!\(index):example.org",
        "name": "Room \(index)", "topic": String(repeating: "x", count: 24 * 1024),
      ]
    }
    let body = try JSONSerialization.data(withJSONObject: ["data": rooms])
    XCTAssertGreaterThan(body.count, ShareLimits.thumbnailBytes)
    XCTAssertLessThan(body.count, ShareLimits.roomListBytes)
    try withHTTP(body: body, declaredBytes: body.count) { http, session in
      let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: root) }
      let store = try ShareStore(root: root)
      try store.publish(session, credential: {})
      let draft = try store.claim(store.create(text: "Synthetic draft").id)
      let sender = TextShareSender(
        store: store, credential: { (session, "synthetic-not-a-credential") }
      ) { descriptor, token, path, method, data, file, mime, limit in
        try http.request(
          descriptor: descriptor, token: token, path: path, method: method, body: data, file: file,
          mime: mime, limit: limit)
      }
      let destinations = try sender.destinations(draft)
      XCTAssertEqual(destinations.count, 150)
      XCTAssertEqual(destinations.last?.title, "Room 149")
      XCTAssertEqual(try store.list().first?.operation.state, .ready)
    }
  }
  func testStreamedResponseLimitIsNotMisclassifiedAsOffline() throws {
    try withHTTP(body: Data(repeating: 32, count: 3 * 1024 * 1024), declaredBytes: nil) {
      http, session in
      XCTAssertThrowsError(
        try http.request(
          descriptor: session, token: "synthetic-not-a-credential", path: "/v1/rooms",
          limit: 2 * 1024 * 1024)
      ) {
        let failure = $0 as? ShareResponseLimitFailure
        XCTAssertEqual(failure?.limit, 2 * 1024 * 1024)
        XCTAssertGreaterThan(failure?.observedBytes ?? 0, 2 * 1024 * 1024)
      }
    }
  }
}
