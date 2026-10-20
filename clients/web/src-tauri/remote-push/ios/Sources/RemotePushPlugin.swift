import Foundation
import Tauri
import UIKit
import UserNotifications
import WebKit

private let tokenTimeoutSeconds: TimeInterval = 15
private let launchPayloadKey = "axon.remote-push.launch"

struct PostLocalArgs: Decodable {
  let title: String
  let body: String
  let collapseId: String
  let accountId: String
  let roomId: String
  let eventId: String
  let threadRootId: String?
}

struct WakePayload: Encodable {
  let account_id: String
  let room_id: String
  let event_id: String
  let thread_root_id: String?
}

/// APNs registration, collapsed local posts, and remote-notification taps.
///
/// The device token is hex-encoded and returned to the page. It is never
/// logged. There is no `remote-notification` background mode and no
/// notification service extension: a suspended app is woken by the alert
/// itself, and a tap is delivered when the process is running again.
class RemotePushPlugin: Plugin, UNUserNotificationCenterDelegate {
  static weak var shared: RemotePushPlugin?
  private var pendingWake: WakePayload?
  private var pendingToken: Invoke?
  private var tokenTimeout: DispatchWorkItem?
  private var tokenHandlersInstalled = false
  private var centerWrapped = false
  private weak var forwardedCenterDelegate: UNUserNotificationCenterDelegate?

  override init() {
    super.init()
    RemotePushPlugin.shared = self
  }

  @objc public override func load(webview: WKWebView) {
    super.load(webview: webview)
    installCenterDelegate()
    if let wake = launchWake() {
      emit(wake)
    }
  }

  @objc public func deviceToken(_ invoke: Invoke) {
    DispatchQueue.main.async {
      self.installCenterDelegate()
      self.installTokenHandlers()
      if self.pendingToken != nil {
        invoke.reject("an APNs token request is already in progress")
        return
      }
      self.pendingToken = invoke
      let timeout = DispatchWorkItem { [weak self] in
        self?.finishToken(nil, failure: "timed out waiting for an APNs device token")
      }
      self.tokenTimeout = timeout
      DispatchQueue.main.asyncAfter(deadline: .now() + tokenTimeoutSeconds, execute: timeout)
      UIApplication.shared.registerForRemoteNotifications()
    }
  }

  @objc public func deliveredEventIds(_ invoke: Invoke) {
    UNUserNotificationCenter.current().getDeliveredNotifications { notifications in
      var ids: [String] = []
      for notification in notifications {
        if let id = eventId(in: notification.request.content.userInfo) {
          ids.append(id)
        }
      }
      invoke.resolve(["eventIds": ids])
    }
  }

  @objc public func pendingWake(_ invoke: Invoke) {
    let wake = pendingWake
    pendingWake = nil
    invoke.resolve(wake)
  }

  @objc public func postLocal(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(PostLocalArgs.self)
    guard isCollapseId(args.collapseId) else {
      invoke.reject("the collapse id is not 32 hex characters")
      return
    }
    let content = UNMutableNotificationContent()
    content.title = args.title
    content.body = args.body
    content.sound = .default
    var info: [String: Any] = [
      "account_id": args.accountId,
      "room_id": args.roomId,
      "event_id": args.eventId,
    ]
    if let thread = args.threadRootId {
      info["thread_root_id"] = thread
    }
    content.userInfo = info
    let request = UNNotificationRequest(
      identifier: args.collapseId, content: content, trigger: nil)
    UNUserNotificationCenter.current().add(request) { error in
      if error == nil {
        invoke.resolve()
      } else {
        invoke.reject("could not post the local notification")
      }
    }
  }

  func didRegister(deviceToken data: Data) {
    finishToken(hex(data), failure: nil)
  }

  func didFailToRegister() {
    finishToken(nil, failure: "APNs registration failed")
  }

  private func finishToken(_ token: String?, failure: String?) {
    DispatchQueue.main.async {
      self.tokenTimeout?.cancel()
      self.tokenTimeout = nil
      guard let invoke = self.pendingToken else {
        return
      }
      self.pendingToken = nil
      if let token = token {
        invoke.resolve([
          "token": token,
          "environment": apnsEnvironment(),
        ])
      } else {
        invoke.reject(failure ?? "APNs registration failed")
      }
    }
  }

  private func installTokenHandlers() {
    if tokenHandlersInstalled {
      return
    }
    guard let delegate = UIApplication.shared.delegate else {
      return
    }
    let cls: AnyClass = type(of: delegate)
    let registerSel = #selector(
      UIApplicationDelegate.application(_:didRegisterForRemoteNotificationsWithDeviceToken:))
    let failSel = #selector(
      UIApplicationDelegate.application(_:didFailToRegisterForRemoteNotificationsWithError:))
    let registered: @convention(block) (AnyObject, UIApplication, Data) -> Void = { _, _, data in
      RemotePushPlugin.shared?.didRegister(deviceToken: data)
    }
    let failed: @convention(block) (AnyObject, UIApplication, Error) -> Void = { _, _, _ in
      RemotePushPlugin.shared?.didFailToRegister()
    }
    let addedRegister = class_addMethod(
      cls, registerSel, imp_implementationWithBlock(registered as Any), "v@:@@")
    let addedFail = class_addMethod(
      cls, failSel, imp_implementationWithBlock(failed as Any), "v@:@@")
    if !addedRegister {
      chain(cls, registerSel, registered)
    }
    if !addedFail {
      chainFail(cls, failSel)
    }
    tokenHandlersInstalled = true
  }

  private func installCenterDelegate() {
    if centerWrapped {
      return
    }
    let center = UNUserNotificationCenter.current()
    let current = center.delegate
    if let current = current, current !== self {
      // The notification plugin drops remote taps. It stays the forward
      // target for the local posts it recorded. A local identifier it did
      // not save crashes its handler, so those stay here.
      forwardedCenterDelegate = current
    }
    center.delegate = self
    centerWrapped = true
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    if notification.request.trigger is UNPushNotificationTrigger {
      completionHandler([])
      return
    }
    if isCollapseId(notification.request.identifier) {
      completionHandler(localPresentation)
      return
    }
    let selector = #selector(
      UNUserNotificationCenterDelegate.userNotificationCenter(_:willPresent:withCompletionHandler:)
    )
    if let forwarded = forwardedCenterDelegate, forwarded.responds(to: selector) {
      forwarded.userNotificationCenter?(
        center, willPresent: notification, withCompletionHandler: completionHandler)
      return
    }
    completionHandler(localPresentation)
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    let identifier = response.notification.request.identifier
    let isRemote = response.notification.request.trigger is UNPushNotificationTrigger
    if response.actionIdentifier != UNNotificationDismissActionIdentifier,
      (isRemote || isCollapseId(identifier)),
      let wake = wake(from: response.notification.request.content.userInfo)
    {
      emit(wake)
      completionHandler()
      return
    }
    let selector = #selector(
      UNUserNotificationCenterDelegate.userNotificationCenter(_:didReceive:withCompletionHandler:)
    )
    if !isRemote, let forwarded = forwardedCenterDelegate, forwarded.responds(to: selector) {
      forwarded.userNotificationCenter?(
        center, didReceive: response, withCompletionHandler: completionHandler)
      return
    }
    completionHandler()
  }

  private func emit(_ wake: WakePayload) {
    pendingWake = wake
    try? trigger("wake", data: wake)
  }

  private func launchWake() -> WakePayload? {
    guard let stored = UserDefaults.standard.dictionary(forKey: launchPayloadKey) else {
      return nil
    }
    UserDefaults.standard.removeObject(forKey: launchPayloadKey)
    var hashed: [AnyHashable: Any] = [:]
    for (key, value) in stored {
      hashed[AnyHashable(key)] = value
    }
    return wake(from: hashed)
  }
}

private var localPresentation: UNNotificationPresentationOptions {
  if #available(iOS 14.0, *) {
    return [.banner, .list, .sound]
  }
  return [.alert, .sound]
}

@_cdecl("init_plugin_remote_push")
func initPlugin() -> Plugin {
  return RemotePushPlugin()
}

private func chain(
  _ cls: AnyClass,
  _ sel: Selector,
  _ registered: @convention(block) (AnyObject, UIApplication, Data) -> Void
) {
  guard let method = class_getInstanceMethod(cls, sel) else {
    return
  }
  let original = method_getImplementation(method)
  typealias Fn = @convention(c) (AnyObject, Selector, UIApplication, Data) -> Void
  let originalFn = unsafeBitCast(original, to: Fn.self)
  let chained: @convention(block) (AnyObject, UIApplication, Data) -> Void = { obj, app, data in
    originalFn(obj, sel, app, data)
    registered(obj, app, data)
  }
  method_setImplementation(method, imp_implementationWithBlock(chained as Any))
}

private func chainFail(_ cls: AnyClass, _ sel: Selector) {
  guard let method = class_getInstanceMethod(cls, sel) else {
    return
  }
  let original = method_getImplementation(method)
  typealias Fn = @convention(c) (AnyObject, Selector, UIApplication, Error) -> Void
  let originalFn = unsafeBitCast(original, to: Fn.self)
  let chained: @convention(block) (AnyObject, UIApplication, Error) -> Void = { obj, app, error in
    originalFn(obj, sel, app, error)
    RemotePushPlugin.shared?.didFailToRegister()
  }
  method_setImplementation(method, imp_implementationWithBlock(chained as Any))
}

private func hex(_ data: Data) -> String {
  data.map { String(format: "%02x", $0) }.joined()
}

private func isCollapseId(_ value: String) -> Bool {
  guard value.count == 32 else {
    return false
  }
  return value.allSatisfy { $0.isHexDigit }
}

private func eventId(in userInfo: [AnyHashable: Any]) -> String? {
  stringValue(userInfo["event_id"]) ?? stringValue(userInfo["eventId"])
}

private func wake(from userInfo: [AnyHashable: Any]) -> WakePayload? {
  let nested = userInfo["aps"] as? [AnyHashable: Any]
  let source = userInfo
  guard
    let account = stringValue(source["account_id"]) ?? stringValue(source["accountId"]),
    let room = stringValue(source["room_id"]) ?? stringValue(source["roomId"]),
    let event = eventId(in: source)
  else {
    if let nested = nested {
      return wake(from: nested)
    }
    return nil
  }
  let thread =
    stringValue(source["thread_root_id"]) ?? stringValue(source["threadRootId"])
  return WakePayload(
    account_id: account, room_id: room, event_id: event, thread_root_id: thread)
}

private func stringValue(_ value: Any?) -> String? {
  guard let text = value as? String else {
    return nil
  }
  let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
  return trimmed.isEmpty ? nil : trimmed
}

/// `development` in the provisioning profile is the API's `sandbox`.
/// TestFlight and the App Store profile say `production`. A missing profile
/// follows the compile configuration, which is what a simulator build has.
private func apnsEnvironment() -> String {
  if let entitlement = provisioningApsEnvironment() {
    return entitlement == "development" ? "sandbox" : "production"
  }
  #if DEBUG
    return "sandbox"
  #else
    return "production"
  #endif
}

private func provisioningApsEnvironment() -> String? {
  guard let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
    let data = try? Data(contentsOf: url),
    let text = String(data: data, encoding: .isoLatin1),
    let key = text.range(of: "<key>aps-environment</key>")
  else {
    return nil
  }
  let after = text[key.upperBound...]
  guard let open = after.range(of: "<string>"), let close = after.range(of: "</string>"),
    open.upperBound <= close.lowerBound
  else {
    return nil
  }
  let value = after[open.upperBound..<close.lowerBound]
    .trimmingCharacters(in: .whitespacesAndNewlines)
  if value == "development" || value == "production" {
    return value
  }
  return nil
}
