import Foundation

/// Small extension-safe HTTP adapter. One ephemeral session per process reuses
/// connections; no cookies, cross-origin redirects, or unbounded response data.
public final class ShareHTTP: NSObject, URLSessionDataDelegate, URLSessionTaskDelegate {
  private final class Pending {
    var data = Data()
    var response: HTTPURLResponse?
    var error: Error?
    let limit: Int
    let done = DispatchSemaphore(value: 0)
    init(limit: Int) { self.limit = limit }
  }
  private let lock = NSLock()
  private let slots = DispatchSemaphore(value: 2)
  private var requests: [Int: Pending] = [:]
  private var session: URLSession!
  public override init() {
    super.init()
    let configuration = URLSessionConfiguration.ephemeral
    configuration.httpCookieStorage = nil
    configuration.urlCache = nil
    configuration.httpMaximumConnectionsPerHost = 2
    configuration.timeoutIntervalForRequest = 30
    configuration.timeoutIntervalForResource = 180
    session = URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
  }

  public func request(
    descriptor: ShareSession, token: String, path: String,
    method: String = "GET", body: Data? = nil, file: URL? = nil,
    mime: String? = nil, limit: Int = 256 * 1024
  ) throws -> (Int, Data) {
    try SharedSession.validateToken(token)
    guard (mime?.utf8.count ?? 0) <= 128,
      mime?.rangeOfCharacter(from: .controlCharacters) == nil
    else { throw ShareFailure.invalid }
    guard slots.wait(timeout: .now() + 3) == .success else { throw ShareFailure.busy }
    defer { slots.signal() }
    guard path.hasPrefix("/v1/"), !path.contains(".."),
      let url = URL(
        string: descriptor.server.trimmingCharacters(in: CharacterSet(charactersIn: "/")) + path),
      ["http", "https"].contains(url.scheme), (1...ShareLimits.thumbnailBytes).contains(limit)
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
    let task: URLSessionTask =
      file.map { session.uploadTask(with: request, fromFile: $0) }
      ?? session.dataTask(with: request)
    lock.lock()
    requests[task.taskIdentifier] = pending
    lock.unlock()
    task.resume()
    guard pending.done.wait(timeout: .now() + deadline + 5) == .success else {
      task.cancel()
      lock.lock()
      requests.removeValue(forKey: task.taskIdentifier)
      lock.unlock()
      throw ShareFailure.expired
    }
    guard pending.error == nil, let response = pending.response else {
      throw ShareFailure.unavailable
    }
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
    guard let pending = requests[dataTask.taskIdentifier],
      response.expectedContentLength <= pending.limit
    else {
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
