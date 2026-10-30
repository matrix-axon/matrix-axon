import Foundation
import Security

public protocol ShareCredentialVault {
  func read() throws -> String
  func write(_ token: String?) throws
}

private final class KeychainVault: ShareCredentialVault {
  private let accessGroup: String
  init(accessGroup: String) { self.accessGroup = accessGroup }
  private var query: [CFString: Any] {
    [
      kSecClass: kSecClassGenericPassword, kSecAttrService: SharedSession.service,
      kSecAttrAccount: "access", kSecAttrAccessGroup: accessGroup,
    ]
  }
  func read() throws -> String {
    var lookup = query
    lookup[kSecReturnData] = true
    lookup[kSecMatchLimit] = kSecMatchLimitOne
    var result: CFTypeRef?
    guard SecItemCopyMatching(lookup as CFDictionary, &result) == errSecSuccess,
      let data = result as? Data, let value = String(data: data, encoding: .utf8), !value.isEmpty
    else { throw ShareFailure.authentication }
    return value
  }
  func write(_ token: String?) throws {
    guard let token else {
      let status = SecItemDelete(query as CFDictionary)
      guard status == errSecSuccess || status == errSecItemNotFound else {
        throw ShareFailure.unavailable
      }
      return
    }
    let attributes: [CFString: Any] = [
      kSecValueData: Data(token.utf8),
      kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
    ]
    let status = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
    if status == errSecItemNotFound {
      let item = query.merging(attributes) { _, new in new }
      guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else {
        throw ShareFailure.unavailable
      }
    } else if status != errSecSuccess {
      throw ShareFailure.unavailable
    }
  }
}

/// A dedicated item exposes an access token only. OAuth refresh credentials
/// remain in the containing app's existing private Keychain service.
public final class SharedSession {
  public static let group = "group.org.matrixaxon.axon.sharing"
  public static let service = "org.matrixaxon.axon.sharing.access"
  private let store: ShareStore
  private let vault: ShareCredentialVault

  public init(store: ShareStore, accessGroup: String, vault: ShareCredentialVault? = nil) {
    self.store = store
    self.vault = vault ?? KeychainVault(accessGroup: accessGroup)
  }

  /// Either barrier suffices: extensions require both a published descriptor
  /// and its credential. Cleanup may fail after dispatch authority is revoked.
  /// The app persists a retry breadcrumb before invoking this operation.
  public static func invalidate(
    fence: () throws -> Void, accessGroup: String?, vault: ShareCredentialVault? = nil
  ) throws {
    var revoked = false
    if let vault = vault ?? accessGroup.map({ KeychainVault(accessGroup: $0) }) {
      do {
        try vault.write(nil)
        revoked = true
      } catch { /* Try the file barrier below. */  }
    }
    do {
      try fence()
      revoked = true
    } catch { /* Retry on next app access. */  }
    guard revoked else { throw ShareFailure.invalidation }
  }

  /// App-only publication. The native generation survives reloads and token
  /// renewal; explicit identity replacement invalidates the old generation.
  public func publish(
    installation: String, server: String, token: String?, expiresAt: Double?,
    replace: Bool
  ) throws -> ShareSession {
    guard let url = URL(string: server), ["https", "http"].contains(url.scheme),
      url.host != nil, url.user == nil, url.password == nil,
      url.query == nil, url.fragment == nil, server.utf8.count <= 4096,
      (token?.utf8.count ?? 0) <= 16 * 1024
    else { throw ShareFailure.invalid }
    if let token { try Self.validateToken(token) }
    return try store.publish(
      make: { previous in
        ShareSession(
          installation: installation,
          generation: !replace && previous?.installation == installation
            && previous?.server == server
            ? previous!.generation : UUID().uuidString,
          server: server, credentialAvailable: token != nil, expiresAt: expiresAt)
      }, credential: { try self.vault.write(token) })
  }

  public func credential() throws -> (ShareSession, String) {
    try store.coordinated {
      guard let descriptor = try store.sessionLocked(), descriptor.credentialAvailable,
        descriptor.expiresAt.map({ $0 > Date().timeIntervalSince1970 + 30 }) ?? true
      else { throw ShareFailure.authentication }
      let token = try vault.read()
      try Self.validateToken(token)
      return (descriptor, token)
    }
  }

  public static func validateToken(_ token: String) throws {
    guard !token.isEmpty, token.utf8.count <= 16 * 1024,
      token.rangeOfCharacter(from: .whitespacesAndNewlines.union(.controlCharacters)) == nil
    else { throw ShareFailure.authentication }
  }

}
