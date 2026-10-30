import Foundation
import XCTest

@testable import AxonIncomingShare

final class SessionTests: XCTestCase {
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
    _ = try store.create(text: "staged while Keychain is unavailable")
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
