import Foundation
import os

/// Fixed numeric diagnostics only; upstream descriptions may contain secrets.
public struct ShareRequestFailure: Error {
  public let status: Int?
  public let networkCode: Int?
  public init(status: Int? = nil, networkCode: Int? = nil) {
    self.status = status
    self.networkCode = networkCode
  }
}

public struct ShareResponseLimitFailure: Error {
  public let limit: Int
  public let observedBytes: Int64
}

/// Small extension-safe HTTP adapter. One ephemeral session per process reuses
/// connections; no cookies, cross-origin redirects, or unbounded response data.
public final class ShareHTTP: NSObject, URLSessionDataDelegate, URLSessionTaskDelegate {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "share-http")
  private final class Pending {
    var data = Data()
    var response: HTTPURLResponse?
    var error: Error?
    var limitExceeded = false
    var observedBytes: Int64 = 0
    let limit: Int
    let done = DispatchSemaphore(value: 0)
    init(limit: Int) { self.limit = limit }
  }
  private let lock = NSLock()
  private let slots = DispatchSemaphore(value: 2)
  private var requests: [Int: Pending] = [:]
  private var closed = false
  private var session: URLSession!
  public override init() {
    super.init()
    configure(.ephemeral)
  }
  init(configuration: URLSessionConfiguration) {
    super.init()
    configure(configuration)
  }
  private func configure(_ configuration: URLSessionConfiguration) {
    configuration.httpCookieStorage = nil
    configuration.urlCache = nil
    configuration.httpMaximumConnectionsPerHost = 2
    configuration.timeoutIntervalForRequest = 30
    configuration.timeoutIntervalForResource = 180
    session = URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
  }

  public func close() {
    lock.lock()
    guard !closed else { lock.unlock(); return }
    closed = true
    lock.unlock()
    session.invalidateAndCancel()
  }

  public func request(
    descriptor: ShareSession, token: String, path: String,
    method: String = "GET", body: Data? = nil, file: URL? = nil,
    mime: String? = nil, limit: Int = 256 * 1024
  ) throws -> (Int, Data) {
    let route =
      path == "/v1/rooms" || path.hasPrefix("/v1/rooms?")
      ? "rooms"
      : path.hasSuffix("/send")
        ? "send"
        : path.hasSuffix("/generate") ? "generate" : "media"
    guard URL(string: descriptor.server)?.scheme == "https" else {
      throw ShareFailure.httpsRequired
    }
    try SharedSession.validateToken(token)
    guard (mime?.utf8.count ?? 0) <= 128,
      mime?.rangeOfCharacter(from: .controlCharacters) == nil
    else { throw ShareFailure.invalid }
    guard slots.wait(timeout: .now() + 3) == .success else { throw ShareFailure.busy }
    defer { slots.signal() }
    guard path.hasPrefix("/v1/"), !path.contains(".."),
      let url = URL(
        string: descriptor.server.trimmingCharacters(in: CharacterSet(charactersIn: "/")) + path),
      ["http", "https"].contains(url.scheme), (1...ShareLimits.responseBytes).contains(limit)
    else { throw ShareFailure.invalid }
    var request = URLRequest(url: url)
    request.httpMethod = method
    let size = try file?.resourceValues(forKeys: [.fileSizeKey]).fileSize
    let deadline = Self.deadline(bytes: size)
    request.timeoutInterval = deadline
    request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
    if let mime, !mime.isEmpty { request.setValue(mime, forHTTPHeaderField: "Content-Type") }
    request.httpBody = body
    let pending = Pending(limit: limit)
    // Closing and task creation/resume share one lock. Invalidation can race
    // a caller that already passed its ownership/cancellation checks.
    lock.lock()
    guard !closed else { lock.unlock(); throw ShareFailure.cancelled }
    let task: URLSessionTask =
      file.map { session.uploadTask(with: request, fromFile: $0) }
      ?? session.dataTask(with: request)
    requests[task.taskIdentifier] = pending
    task.resume()
    lock.unlock()
    guard pending.done.wait(timeout: .now() + deadline + 5) == .success else {
      task.cancel()
      lock.lock()
      requests.removeValue(forKey: task.taskIdentifier)
      lock.unlock()
      throw ShareFailure.expired
    }
    if pending.limitExceeded {
      throw ShareResponseLimitFailure(limit: limit, observedBytes: pending.observedBytes)
    }
    guard pending.error == nil, let response = pending.response else {
      let code = (pending.error as? URLError)?.code.rawValue ?? 0
      Self.log.error(
        "share_http_failed route=\(route, privacy: .public) url_error=\(code) bytes=\(pending.data.count)"
      )
      throw ShareRequestFailure(networkCode: code)
    }
    Self.log.info(
      "share_http_response route=\(route, privacy: .public) status=\(response.statusCode) bytes=\(pending.data.count)"
    )
    return (response.statusCode, pending.data)
  }

  static func deadline(bytes: Int?) -> TimeInterval {
    guard let bytes else { return 30 }
    return min(180, max(30, 20 + Double(max(0, bytes)) / (256 * 1024)))
  }

  public func urlSession(
    _ session: URLSession, dataTask: URLSessionDataTask,
    didReceive response: URLResponse,
    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
  ) {
    lock.lock()
    defer { lock.unlock() }
    guard let pending = requests[dataTask.taskIdentifier] else {
      completionHandler(.cancel)
      return
    }
    if response.expectedContentLength > pending.limit {
      pending.limitExceeded = true
      pending.observedBytes = response.expectedContentLength
      completionHandler(.cancel)
      return
    }
    pending.response = response as? HTTPURLResponse
    completionHandler(.allow)
  }
  public func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data)
  {
    lock.lock()
    defer { lock.unlock() }
    guard let pending = requests[dataTask.taskIdentifier] else { return }
    if pending.data.count + data.count > pending.limit {
      pending.limitExceeded = true
      pending.observedBytes = Int64(pending.data.count) + Int64(data.count)
      dataTask.cancel()
      return
    }
    pending.data.append(data)
  }
  public func urlSession(
    _ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?
  ) {
    lock.lock()
    let pending = requests.removeValue(forKey: task.taskIdentifier)
    lock.unlock()
    pending?.error = error
    pending?.done.signal()
  }
  public func urlSession(
    _ session: URLSession, task: URLSessionTask,
    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
    completionHandler: @escaping (URLRequest?) -> Void
  ) {
    // No request follows a redirect, so credentials never leave the selected origin.
    completionHandler(nil)
  }
}
