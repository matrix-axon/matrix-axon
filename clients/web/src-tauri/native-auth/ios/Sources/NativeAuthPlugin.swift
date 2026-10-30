import AuthenticationServices
import AxonIncomingShare
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
  private let shareQueue = DispatchQueue(label: "org.matrixaxon.axon.incoming-share")
  private let shareTransfers = DispatchQueue(label: "org.matrixaxon.axon.share-transfers", attributes: .concurrent)
  private let shareTransferSlots = DispatchSemaphore(value: 2)
  private let sharingSetup = NSLock()
  private var shareStore: ShareStore?
  private var sharedSession: SharedSession?
  private var installation: String?
  private let shareHTTP = ShareHTTP()
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

  /// The containing app owns install initialization. This method is never
  /// linked into the extension-safe module or called by an extension.
  private func prepareSharing() throws -> (ShareStore, SharedSession, String) {
    sharingSetup.lock()
    defer { sharingSetup.unlock() }
    if let store = shareStore, let session = sharedSession, let install = installation {
      return (store, session, install)
    }
    guard let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: SharedSession.group),
          let accessGroup = Bundle.main.object(forInfoDictionaryKey: "AxonSharedKeychainGroup") as? String,
          !accessGroup.contains("$(") else { throw ShareFailure.unavailable }
    let defaults = UserDefaults.standard
    let markerKey = "org.matrixaxon.axon.sharing.installation"
    let previous = defaults.string(forKey: markerKey)
    // The old marker distinguishes an upgrade from a reinstall. It is app
    // private and is never inferred from inherited Keychain data.
    let upgraded = defaults.bool(forKey: Keychain.installMarker)
    let install = previous ?? UUID().uuidString
    let store = try ShareStore(root: container.appendingPathComponent("IncomingShares", isDirectory: true))
    let session = SharedSession(store: store, accessGroup: accessGroup)
    if previous == nil {
      _ = try session.publish(installation: install, server: "https://unconfigured.invalid",
                              token: nil, expiresAt: nil, replace: true)
      if !upgraded { Keychain.forgetPreviousInstall() }
      defaults.set(install, forKey: markerKey)
    }
    shareStore = store; sharedSession = session; installation = install
    return (store, session, install)
  }

  @objc public func share(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(ShareArgs.self)
    let transfer = ["upload", "retain", "cleanup"].contains(args.action)
    if transfer && shareTransferSlots.wait(timeout: .now()) != .success {
      invoke.reject("Incoming share: busy", code: "busy")
      return
    }
    let queue = transfer ? shareTransfers : shareQueue
    queue.async {
      defer { if transfer { self.shareTransferSlots.signal() } }
      do {
        let (store, session, install) = try self.prepareSharing()
        var result: Any = [:]
        switch args.action {
        case "publish":
          guard let server = args.server else { throw ShareFailure.invalid }
          result = try self.shareObject(session.publish(installation: install, server: server,
            token: args.token, expiresAt: args.expiresAt.map { $0 / 1000 }, replace: args.replace ?? false))
        case "list": result = try self.shareObject(store.list())
        case "clear_account": try store.clearAccount(args.accountId ?? "")
        case "claim": result = try self.shareObject(store.claim(args.id ?? ""))
        case "update":
          guard let manifest = args.manifest else { throw ShareFailure.invalid }
          result = try self.shareObject(store.update(manifest, release: args.release ?? false))
        case "discard", "ack":
          try store.discard(args.id ?? "", owner: args.owner, completed: args.action == "ack")
        case "read":
          let data = try store.readFile(args.id ?? "", file: args.file ?? "", owner: args.owner ?? "",
                                        offset: args.offset ?? 0, count: args.count ?? 0)
          result = ["bytes": data.base64EncodedString()]
        case "cleanup":
          guard let account = args.accountId, UUID(uuidString: account) != nil,
                let uploads = args.uploadIds, uploads.count <= 18,
                uploads.allSatisfy({ UUID(uuidString: $0) != nil }),
                let descriptor = try store.session(), descriptor.generation == args.generation
          else { throw ShareFailure.stale }
          let token = try args.token ?? session.credential().1
          let deadline = Date().addingTimeInterval(30)
          for upload in uploads {
            guard Date() < deadline, try store.session()?.generation == descriptor.generation else { break }
            _ = try? self.shareHTTP.request(descriptor: descriptor, token: token,
              path: "/v1/accounts/\(account)/media/uploads/\(upload)", method: "DELETE", limit: 4096)
          }
        case "upload", "retain":
          guard let id = args.id, let owner = args.owner, let account = args.accountId,
                UUID(uuidString: account) != nil else { throw ShareFailure.invalid }
          let descriptor: ShareSession
          let token: String
          if let transient = args.token {
            // Main-app recovery may use a one-time session. Consume its bearer
            // only for this request, never publish it to shared credentials.
            try SharedSession.validateToken(transient)
            guard let current = try store.session() else { throw ShareFailure.authentication }
            descriptor = current; token = transient
          } else {
            (descriptor, token) = try session.credential()
          }
          let manifest = try store.list().first { $0.id == id }
          guard manifest?.session == descriptor.generation, manifest?.owner == owner,
                manifest?.accountId == account else { throw ShareFailure.stale }
          if args.action == "upload" {
            let (url, file) = try store.verifiedFile(id, file: args.file ?? "", owner: owner)
            let kind = file.thumbnail || file.mime.hasPrefix("image/") ? "image" : "file"
            let filename = file.name.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? "attachment"
            let (status, body) = try self.shareHTTP.request(descriptor: descriptor, token: token,
              path: "/v1/accounts/\(account)/media/uploads?kind=\(kind)&filename=\(filename)",
              method: "POST", file: url, mime: file.mime)
            guard (200..<300).contains(status),
                  let envelope = try JSONSerialization.jsonObject(with: body) as? [String: Any],
                  let data = envelope["data"] as? [String: Any], let upload = data["upload_id"] as? String,
                  UUID(uuidString: upload) != nil else { throw status == 401 ? ShareFailure.authentication : ShareFailure.unavailable }
            guard try store.session()?.generation == descriptor.generation else { throw ShareFailure.stale }
            result = ["uploadId": upload]
          } else {
            guard let upload = args.uploadId, UUID(uuidString: upload) != nil else { throw ShareFailure.invalid }
            let (status, body) = try self.shareHTTP.request(descriptor: descriptor, token: token,
              path: "/v1/accounts/\(account)/media/uploads/\(upload)/preview", limit: ShareLimits.thumbnailBytes)
            guard status == 200 else { throw status == 404 ? ShareFailure.missingFile : ShareFailure.unavailable }
            let mime: String
            if body.starts(with: [0x89, 0x50, 0x4e, 0x47]) { mime = "image/png" }
            else if body.starts(with: [0xff, 0xd8, 0xff]) { mime = "image/jpeg" }
            else if body.starts(with: Array("RIFF".utf8)) { mime = "image/webp" }
            else { throw ShareFailure.invalid }
            result = try self.shareObject(store.importThumbnail(id, owner: owner, data: body, mime: mime))
          }
        #if DEBUG
        case "seed": result = try self.shareObject(store.create(text: args.text ?? ""))
        #endif
        default: throw ShareFailure.invalid
        }
        invoke.resolve(["value": result])
      } catch {
        let code = (error as? ShareFailure)?.rawValue ?? (error is DecodingError ? "invalid" : "unavailable")
        invoke.reject("Incoming share: \(code)", code: code)
      }
    }
  }

  private func shareObject<T: Encodable>(_ value: T) throws -> Any {
    try JSONSerialization.jsonObject(with: ShareJSON.encode(value), options: [.fragmentsAllowed])
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

private struct ShareArgs: Decodable {
  let action: String
  let server: String?
  let token: String?
  let expiresAt: Double?
  let replace: Bool?
  let id: String?
  let owner: String?
  let manifest: ShareManifest?
  let release: Bool?
  let file: String?
  let offset: Int?
  let count: Int?
  let text: String?
  let accountId: String?
  let uploadId: String?
  let uploadIds: [String]?
  let generation: String?
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
