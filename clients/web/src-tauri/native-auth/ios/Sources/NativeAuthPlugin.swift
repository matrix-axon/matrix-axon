import AuthenticationServices
import Security
import SwiftRs
import Tauri
import UIKit
import WebKit

struct AppleSignInArgs: Decodable {
  let nonce: String
}

struct SecretKeyArgs: Decodable {
  let key: String
}

struct SecretSetArgs: Decodable {
  let key: String
  let value: String
}

/// Sign in with Apple and Keychain storage for the Axon shell (ADR 0054).
///
/// Errors carry Apple's error codes and OSStatus numbers only. Nothing here
/// logs or echoes a token, a nonce, or a stored value.
class NativeAuthPlugin: Plugin {
  /// The one Sign in with Apple request in flight. `ASAuthorizationController`
  /// holds its delegate weakly, so without this the delegate is released as
  /// soon as `appleSignIn` returns and the result is never delivered.
  private var pending: AppleSignInRequest?

  @objc public func appleSignIn(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(AppleSignInArgs.self)
    DispatchQueue.main.async {
      if self.pending != nil {
        invoke.reject("a Sign in with Apple request is already in progress")
        return
      }
      let request = ASAuthorizationAppleIDProvider().createRequest()
      // No scopes. Axon never uses Apple's name or email: ownership is the
      // signed subject alone, bound explicitly (docs/apple-oauth-native.md), so
      // asking for profile data would only collect something to discard.
      request.requestedScopes = []
      // Verbatim. The server's nonce is already a digest of its own randomness,
      // and the server checks the signed claim against exactly this string.
      request.nonce = args.nonce
      let anchor = self.presentationAnchor()
      let pending = AppleSignInRequest(
        request: request, invoke: invoke, anchor: anchor
      ) { [weak self] in
        self?.pending = nil
      }
      self.pending = pending
      pending.start()
    }
  }

  private func presentationAnchor() -> ASPresentationAnchor {
    if let window = manager.viewController?.view.window {
      return window
    }
    let windows = UIApplication.shared.connectedScenes
      .compactMap { $0 as? UIWindowScene }
      .flatMap { $0.windows }
    return windows.first(where: { $0.isKeyWindow }) ?? windows.first ?? ASPresentationAnchor()
  }

  @objc public func secretLoad(_ invoke: Invoke) {
    Keychain.forgetPreviousInstall()
    switch Keychain.loadAll() {
    case .success(let entries):
      invoke.resolve(["entries": entries])
    case .failure(let status):
      invoke.reject("Keychain read failed (OSStatus \(status.code))")
    }
  }

  @objc public func secretSet(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SecretSetArgs.self)
    let status = Keychain.set(args.key, args.value)
    if status == errSecSuccess {
      invoke.resolve()
    } else {
      invoke.reject("Keychain write failed (OSStatus \(status))")
    }
  }

  @objc public func secretDelete(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SecretKeyArgs.self)
    let status = Keychain.delete(args.key)
    if status == errSecSuccess || status == errSecItemNotFound {
      invoke.resolve()
    } else {
      invoke.reject("Keychain delete failed (OSStatus \(status))")
    }
  }
}

/// One `ASAuthorizationController` run, owning everything it needs until the
/// sheet reports back exactly once.
private class AppleSignInRequest: NSObject, ASAuthorizationControllerDelegate,
  ASAuthorizationControllerPresentationContextProviding
{
  private let controller: ASAuthorizationController
  private let invoke: Invoke
  private let anchor: ASPresentationAnchor
  private let done: () -> Void

  init(
    request: ASAuthorizationAppleIDRequest, invoke: Invoke, anchor: ASPresentationAnchor,
    done: @escaping () -> Void
  ) {
    self.controller = ASAuthorizationController(authorizationRequests: [request])
    self.invoke = invoke
    self.anchor = anchor
    self.done = done
    super.init()
  }

  func start() {
    controller.delegate = self
    controller.presentationContextProvider = self
    controller.performRequests()
  }

  func presentationAnchor(for controller: ASAuthorizationController) -> ASPresentationAnchor {
    anchor
  }

  func authorizationController(
    controller: ASAuthorizationController,
    didCompleteWithAuthorization authorization: ASAuthorization
  ) {
    defer { done() }
    guard
      let credential = authorization.credential as? ASAuthorizationAppleIDCredential,
      let data = credential.identityToken,
      let token = String(data: data, encoding: .utf8)
    else {
      invoke.reject("Apple returned no identity token")
      return
    }
    invoke.resolve(["identityToken": token])
  }

  func authorizationController(
    controller: ASAuthorizationController, didCompleteWithError error: Error
  ) {
    defer { done() }
    let code = (error as? ASAuthorizationError)?.code
    switch code {
    case .canceled:
      invoke.reject("Sign in with Apple was cancelled", code: "cancelled")
    case .unknown:
      // What an unentitled build gets: the sheet never appears and Apple
      // reports "unknown" (1000). Say where to look rather than "unknown".
      invoke.reject(
        "Sign in with Apple is unavailable to this build (error 1000). Check the app's Sign in with Apple entitlement and provisioning profile, and that the device is signed in to an Apple Account."
      )
    default:
      let number = (error as NSError).code
      invoke.reject("Sign in with Apple failed (error \(number))")
    }
  }
}

/// Generic-password items under one service, readable after the first unlock
/// following a boot and never migrated to another device or into a backup.
private enum Keychain {
  static let service = "org.matrixaxon.axon.auth"
  static let installMarker = "org.matrixaxon.axon.auth.keychain-owned"

  struct Status: Error {
    let code: OSStatus
  }

  /// Keychain items outlive the app: deleting and reinstalling it leaves them
  /// in place, where the fresh install would find them and sign in as whoever
  /// used the device before. `UserDefaults` does not survive a reinstall, so
  /// its absence means these items belong to an earlier install.
  static func forgetPreviousInstall() {
    let defaults = UserDefaults.standard
    if defaults.bool(forKey: installMarker) {
      return
    }
    SecItemDelete(
      [
        kSecClass: kSecClassGenericPassword,
        kSecAttrService: service,
      ] as CFDictionary)
    defaults.set(true, forKey: installMarker)
  }

  static func loadAll() -> Result<[String: String], Status> {
    var result: CFTypeRef?
    let status = SecItemCopyMatching(
      [
        kSecClass: kSecClassGenericPassword,
        kSecAttrService: service,
        kSecMatchLimit: kSecMatchLimitAll,
        kSecReturnAttributes: true,
        kSecReturnData: true,
      ] as CFDictionary, &result)
    if status == errSecItemNotFound {
      return .success([:])
    }
    guard status == errSecSuccess, let items = result as? [[CFString: Any]] else {
      return .failure(Status(code: status))
    }
    var entries: [String: String] = [:]
    for item in items {
      if let key = item[kSecAttrAccount] as? String,
        let data = item[kSecValueData] as? Data,
        let value = String(data: data, encoding: .utf8)
      {
        entries[key] = value
      }
    }
    return .success(entries)
  }

  /// Update in place, and add only when there is nothing to update. Deleting
  /// first would leave a moment with no token at all, and a crash there would
  /// sign the user out.
  static func set(_ key: String, _ value: String) -> OSStatus {
    let data = Data(value.utf8)
    let query: [CFString: Any] = [
      kSecClass: kSecClassGenericPassword,
      kSecAttrService: service,
      kSecAttrAccount: key,
    ]
    let updated = SecItemUpdate(
      query as CFDictionary,
      [
        kSecValueData: data,
        kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
      ] as CFDictionary)
    if updated != errSecItemNotFound {
      return updated
    }
    var add = query
    add[kSecValueData] = data
    add[kSecAttrAccessible] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
    return SecItemAdd(add as CFDictionary, nil)
  }

  static func delete(_ key: String) -> OSStatus {
    SecItemDelete(
      [
        kSecClass: kSecClassGenericPassword,
        kSecAttrService: service,
        kSecAttrAccount: key,
      ] as CFDictionary)
  }
}

@_cdecl("init_plugin_native_auth")
func initPlugin() -> Plugin {
  return NativeAuthPlugin()
}
