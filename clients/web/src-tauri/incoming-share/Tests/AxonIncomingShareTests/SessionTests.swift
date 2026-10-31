import Foundation
import XCTest

@testable import AxonIncomingShare

final class SessionTests: XCTestCase {
  func testInvalidatedSessionCannotStageNewShares() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    try store.publish(
      ShareSession(
        installation: "install", server: "https://axon.example",
        credentialAvailable: true, expiresAt: nil), credential: {})
    try store.invalidateSession()
    XCTAssertThrowsError(try store.create(text: "must not report a saved draft"))
    XCTAssertTrue(try store.list().isEmpty)
  }
  func testKilledPublicationIntentFencesAuthorityAndHealsOldRecords() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let vault = Vault()
    let session = SharedSession(store: store, accessGroup: "test", vault: vault)
    var next = try session.publish(
      installation: "install", server: "https://axon.example", token: "old-access", expiresAt: nil,
      replace: false)
    let old = try store.claim(store.create(text: "old reader").id)
    next.generation = UUID().uuidString
    try ShareJSON.encode(next).write(to: root.appendingPathComponent("session.pending.json"))
    XCTAssertNil(try store.session())
    XCTAssertThrowsError(try session.credential())
    XCTAssertThrowsError(try store.update(old))
    XCTAssertTrue(try store.list().isEmpty)
    try store.publish(next, credential: {})
    XCTAssertEqual(try store.session()?.generation, next.generation)
    XCTAssertTrue(try store.list().isEmpty)
  }
  func testRevocationSurvivesEitherUnavailableBoundaryButNeverBoth() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let vault = Vault()
    let session = SharedSession(store: store, accessGroup: "test", vault: vault)
    _ = try session.publish(
      installation: "install", server: "https://axon.example", token: "synthetic", expiresAt: nil,
      replace: false)
    _ = try store.create(text: "draft")
    vault.unavailable = true
    try SharedSession.invalidate(
      fence: { try store.invalidateSession() }, accessGroup: "test", vault: vault)
    XCTAssertNil(try store.session())
    XCTAssertThrowsError(try session.credential())
    XCTAssertTrue(try store.list().isEmpty)
    XCTAssertThrowsError(
      try SharedSession.invalidate(
        fence: { throw ShareFailure.busy }, accessGroup: "test", vault: vault))
    vault.unavailable = false
    try SharedSession.invalidate(
      fence: { throw ShareFailure.unavailable }, accessGroup: "test", vault: vault)
    XCTAssertNil(vault.token)
  }
  func testHTTPSharingRejectsBeforeAnyNetworkRequest() {
    let descriptor = ShareSession(
      installation: "install", server: "http://example.org", credentialAvailable: true,
      expiresAt: nil)
    XCTAssertThrowsError(
      try ShareHTTP().request(descriptor: descriptor, token: "synthetic", path: "/v1/rooms")
    ) {
      XCTAssertEqual($0 as? ShareFailure, .httpsRequired)
    }
  }
  func testTransferBudgetsStayBoundedAcrossSupportedSizes() {
    XCTAssertLessThanOrEqual(ShareHTTP.deadline(bytes: ShareLimits.thumbnailBytes), 30)
    XCTAssertLessThanOrEqual(ShareHTTP.deadline(bytes: ShareLimits.batchBytes), 180)
    XCTAssertEqual(ShareHTTP.deadline(bytes: nil), 30)
  }
  final class Vault: ShareCredentialVault {
    var token: String?
    var unavailable = false
    func read() throws -> String {
      guard !unavailable, let token else { throw ShareFailure.authentication }
      return token
    }
    func write(_ token: String?) throws {
      if unavailable { throw ShareFailure.unavailable }
      self.token = token
    }
  }
  func testPublicationFailureRetainsDraftAndCanReconcile() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let vault = Vault()
    let session = SharedSession(store: store, accessGroup: "test", vault: vault)
    let first = try session.publish(
      installation: "install", server: "https://axon.example",
      token: "old-access", expiresAt: nil, replace: false)
    let draft = try store.create(text: "hello")
    vault.unavailable = true
    XCTAssertThrowsError(
      try session.publish(
        installation: "install", server: "https://axon.example",
        token: "new-access", expiresAt: nil, replace: false))
    XCTAssertThrowsError(try session.credential())
    XCTAssertEqual(try store.list().map(\.id), [draft.id])
    XCTAssertThrowsError(try store.create(text: "new staging waits for publication recovery"))
    vault.unavailable = false
    let refreshed = try session.publish(
      installation: "install", server: "https://axon.example",
      token: "new-access", expiresAt: nil, replace: false)
    XCTAssertEqual(first.generation, refreshed.generation)
    XCTAssertNotEqual(first.revision, refreshed.revision)
    XCTAssertEqual(try session.credential().1, "new-access")
    let files = try FileManager.default.contentsOfDirectory(
      at: root, includingPropertiesForKeys: nil)
    for file in files where file.pathExtension == "json" {
      let content = try String(contentsOf: file)
      XCTAssertFalse(content.contains("new-access"))
      XCTAssertFalse(content.contains("old-access"))
    }
  }

  func testBeforeFirstAppLaunchCannotConsumeInheritedCredential() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let vault = Vault()
    vault.token = "inherited-access"
    let session = SharedSession(store: store, accessGroup: "test", vault: vault)
    XCTAssertThrowsError(try session.credential())
    XCTAssertEqual(vault.token, "inherited-access")  // extension never performs cleanup
    _ = try session.publish(
      installation: "new-install", server: "https://axon.example", token: nil, expiresAt: nil,
      replace: true)
    XCTAssertNil(vault.token)
  }

  func testPublishedSignedOutSessionRefusesStagingButSessionOnlySignInAllowsIt() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let session = SharedSession(store: store, accessGroup: "test", vault: Vault())
    _ = try session.publish(
      installation: "install", server: "https://axon.example",
      token: nil, expiresAt: nil, replace: true, stagingAllowed: false)
    XCTAssertThrowsError(try store.create(text: "signed out"))
    _ = try session.publish(
      installation: "install", server: "https://axon.example",
      token: nil, expiresAt: nil, replace: true, stagingAllowed: true)
    XCTAssertNoThrow(try store.create(text: "recover in the main app"))
    XCTAssertThrowsError(try session.credential())
  }

  func testMissingSessionTransportDoesNotRecommendHTTPS() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    XCTAssertThrowsError(try store.checkTransport()) {
      XCTAssertEqual($0 as? ShareFailure, .authentication)
    }
    try store.publish(
      ShareSession(
        installation: "install", server: "http://axon.example",
        credentialAvailable: true, expiresAt: nil), credential: {})
    XCTAssertThrowsError(try store.checkTransport()) {
      XCTAssertEqual($0 as? ShareFailure, .httpsRequired)
    }
    try store.invalidateSession()
    XCTAssertThrowsError(try store.checkTransport()) {
      XCTAssertEqual($0 as? ShareFailure, .authentication)
    }
  }

  func testExpiredAndSessionOnlyCredentialsPreserveDraft() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ShareStore(root: root)
    let session = SharedSession(store: store, accessGroup: "test", vault: Vault())
    _ = try session.publish(
      installation: "install", server: "https://axon.example", token: "expired", expiresAt: 1,
      replace: false)
    _ = try store.create(text: "draft")
    XCTAssertThrowsError(try session.credential())
    _ = try session.publish(
      installation: "install", server: "https://axon.example", token: nil, expiresAt: nil,
      replace: false)
    XCTAssertThrowsError(try session.credential())
    XCTAssertEqual(try store.list().count, 1)
    _ = try session.publish(
      installation: "install", server: "https://another.example", token: nil, expiresAt: nil,
      replace: false)
    XCTAssertTrue(try store.list().isEmpty)
  }
}
