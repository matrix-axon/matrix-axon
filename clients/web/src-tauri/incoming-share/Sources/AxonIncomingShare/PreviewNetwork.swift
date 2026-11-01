import CFNetwork
import Darwin
import Foundation
import Network
import Security
import dnssd

/// Dedicated, credential-free preview transport. DNS is cancellable; the TLS
/// connection receives only a validated numeric endpoint, never the hostname.
/// Hostname authentication still uses the original name. No URLSession, proxy,
/// cookies, certificate network fetches, script engine or automatic redirects.
enum PreviewNetwork {
  struct Response {
    let status: Int
    let headers: [String: String]
    let body: Data
    let url: URL
  }
  enum Kind { case page, image }
  static let headBytes = 256 * 1024
  static let headerBytes = 32 * 1024

  struct Address: Equatable {
    let text: String
    let bytes: [UInt8]
    private init(text: String, bytes: [UInt8]) {
      self.text = text
      self.bytes = bytes
    }
    static func parse(_ text: String) -> Address? {
      var v4 = in_addr()
      if inet_pton(AF_INET, text, &v4) == 1 {
        return Address(text: text, bytes: withUnsafeBytes(of: &v4) { Array($0) })
      }
      var v6 = in6_addr()
      if inet_pton(AF_INET6, text, &v6) == 1 {
        return Address(text: text, bytes: withUnsafeBytes(of: &v6) { Array($0) })
      }
      return nil
    }
    var isPublic: Bool {
      let b = bytes
      if b.count == 4 {
        guard b[0] > 0, b[0] < 224 else { return false }
        if [10, 127].contains(b[0]) { return false }
        if b[0] == 100 && (64...127).contains(b[1]) { return false }
        if b[0] == 169 && b[1] == 254 { return false }
        if b[0] == 172 && (16...31).contains(b[1]) { return false }
        if b[0] == 192
          && (b[1] == 168 || (b[1] == 0 && [0, 2].contains(b[2])) || (b[1] == 88 && b[2] == 99))
        {
          return false
        }
        if b[0] == 198 && ([18, 19].contains(b[1]) || (b[1] == 51 && b[2] == 100)) { return false }
        return !(b[0] == 203 && b[1] == 0 && b[2] == 113)
      }
      // Well-known NAT64 prefix: allow only a public embedded IPv4 address.
      if Array(b.prefix(12)) == [0, 100, 255, 155, 0, 0, 0, 0, 0, 0, 0, 0] {
        return Address(text: text, bytes: Array(b.suffix(4))).isPublic
      }
      guard b.count == 16, b[0] & 0xe0 == 0x20 else { return false }
      // Special-purpose, transition, documentation and former 6bone ranges.
      if b[0] == 0x20 && b[1] == 1 && (b[2] < 2 || (b[2] == 0x0d && b[3] == 0xb8)) { return false }
      if b[0] == 0x20 && b[1] == 2 { return false }
      if b[0] == 0x3f && (b[1] == 0xfe || (b[1] == 0xff && b[2] & 0xf0 == 0)) { return false }
      return true
    }
    var endpoint: NWEndpoint.Host {
      bytes.count == 4 ? .ipv4(IPv4Address(text)!) : .ipv6(IPv6Address(text)!)
    }
  }

  static func validate(_ url: URL) throws -> String {
    let raw = url.absoluteString
    guard TextShare.previewURL(raw),
      let c = URLComponents(url: url, resolvingAgainstBaseURL: false),
      c.scheme == "https", let rawHost = c.host?.lowercased()
    else { throw ShareFailure.previewUnavailable }
    let host = rawHost.hasSuffix(".") ? String(rawHost.dropLast()) : rawHost
    let labels = host.split(separator: ".", omittingEmptySubsequences: false)
    guard host.utf8.count <= 253, host.contains("."),
      host.utf8.allSatisfy({
        (97...122).contains($0) || (48...57).contains($0) || $0 == 45 || $0 == 46
      }),
      labels.allSatisfy({
        !$0.isEmpty && $0.utf8.count <= 63 && !$0.hasPrefix("-") && !$0.hasSuffix("-")
      }),
      labels.last?.allSatisfy(\.isNumber) == false,
      Address.parse(host) == nil, !host.contains(":"),
      c.port.map({ (1...65535).contains($0) }) ?? true,
      ![
        "local", "localhost", "home", "lan", "corp", "internal", "localdomain", "intranet",
        "private", "onion", "i2p", "invalid", "test", "example", "alt", "arpa",
      ].contains(host.split(separator: ".").last.map(String.init) ?? "")
    else { throw ShareFailure.previewUnavailable }
    return host
  }

  typealias Resolve = (String, TimeInterval, ShareCancellation) throws -> Address
  typealias Exchange = (URL, String, Address, Kind, TimeInterval, ShareCancellation) throws ->
    Response

  static func fetch(
    _ initial: URL, kind: Kind, deadline: DispatchTime, cancellation: ShareCancellation,
    resolve: Resolve = resolve, exchange: Exchange = exchange
  ) throws -> Response {
    var url = initial
    for _ in 0..<6 {
      try cancellation.check()
      let host = try validate(url)
      let address = try resolve(host, min(4, remaining(deadline)), cancellation)
      guard address.isPublic else { throw ShareFailure.previewUnavailable }
      let response = try exchange(url, host, address, kind, remaining(deadline), cancellation)
      if [301, 302, 303, 307, 308].contains(response.status) {
        guard let location = response.headers["location"], location.utf8.count <= 4096,
          let next = URL(string: location, relativeTo: url)?.absoluteURL
        else { throw ShareFailure.invalidResponse }
        _ = try validate(next)
        url = next
        continue
      }
      guard response.status == 200 else { throw ShareFailure.previewUnavailable }
      return response
    }
    throw ShareFailure.previewUnavailable
  }

  static func remaining(_ deadline: DispatchTime) throws -> TimeInterval {
    let now = DispatchTime.now().uptimeNanoseconds
    guard deadline.uptimeNanoseconds > now else { throw ShareFailure.expired }
    return Double(deadline.uptimeNanoseconds - now) / 1_000_000_000
  }

  private final class DNSResult {
    let result = ShareIntakeResult<Address>()
    var reference: DNSServiceRef?
  }
  // One worker per active lookup; deallocation is serialized with callbacks.
  static func resolve(_ host: String, timeout: TimeInterval, cancellation: ShareCancellation) throws
    -> Address
  {
    let queue = DispatchQueue(label: "org.matrixaxon.axon.preview-dns")
    let state = DNSResult()
    let deadline = DispatchTime.now() + timeout
    queue.async {
      do { try cancellation.check() } catch {
        state.result.finish(.failure(error))
        return
      }
      guard DispatchTime.now() < deadline else {
        state.result.finish(.failure(ShareFailure.expired))
        return
      }
      let status = DNSServiceGetAddrInfo(
        &state.reference, 0, 0,
        DNSServiceProtocol(kDNSServiceProtocol_IPv4 | kDNSServiceProtocol_IPv6), host,
        { _, flags, _, error, _, address, _, context in
          guard let context else { return }
          let state = Unmanaged<DNSResult>.fromOpaque(context).takeUnretainedValue()
          if error == kDNSServiceErr_NoSuchRecord { return }
          guard error == 0, flags & kDNSServiceFlagsAdd != 0, let address else {
            state.result.finish(.failure(ShareFailure.unavailable))
            return
          }
          var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
          guard
            getnameinfo(
              address, socklen_t(address.pointee.sa_len), &buffer, socklen_t(buffer.count), nil, 0,
              NI_NUMERICHOST) == 0,
            let parsed = Address.parse(String(cString: buffer)), parsed.isPublic
          else {
            state.result.finish(.failure(ShareFailure.previewUnavailable))
            return
          }
          state.result.finish(.success(parsed))
        }, Unmanaged.passUnretained(state).toOpaque())
      if status == 0, let reference = state.reference {
        if DNSServiceSetDispatchQueue(reference, queue) != 0 {
          state.result.finish(.failure(ShareFailure.unavailable))
        }
      } else {
        state.result.finish(.failure(ShareFailure.unavailable))
      }
    }
    defer {
      queue.async {
        if let reference = state.reference {
          DNSServiceRefDeallocate(reference)
          state.reference = nil
        }
      }
    }
    return try state.result.wait(timeout: timeout, cancellation: cancellation)
  }

  static func exchange(
    _ url: URL, host: String, address: Address, kind: Kind,
    timeout: TimeInterval, cancellation: ShareCancellation
  ) throws -> Response {
    try cancellation.check()
    guard try validate(url) == host, address.isPublic,
      let components = URLComponents(url: url, resolvingAgainstBaseURL: false)
    else { throw ShareFailure.previewUnavailable }
    let tls = NWProtocolTLS.Options()
    sec_protocol_options_set_tls_server_name(tls.securityProtocolOptions, host)
    sec_protocol_options_add_tls_application_protocol(tls.securityProtocolOptions, "http/1.1")
    sec_protocol_options_set_min_tls_protocol_version(tls.securityProtocolOptions, .TLSv12)
    let verifyQueue = DispatchQueue(label: "org.matrixaxon.axon.preview-trust")
    sec_protocol_options_set_verify_block(
      tls.securityProtocolOptions,
      { _, trust, complete in
        let value = sec_trust_copy_ref(trust).takeRetainedValue()
        complete(verify(value, host: host))
      }, verifyQueue)
    let parameters = NWParameters(tls: tls, tcp: NWProtocolTCP.Options())
    parameters.preferNoProxies = true
    let port = NWEndpoint.Port(rawValue: UInt16(components.port ?? 443))!
    let connection = NWConnection(host: address.endpoint, port: port, using: parameters)
    let target =
      (components.percentEncodedPath.isEmpty ? "/" : components.percentEncodedPath)
      + (components.percentEncodedQuery.map { "?" + $0 } ?? "")
    let authority = host + (components.port.map { ":\($0)" } ?? "")
    let request = Data(
      "GET \(target) HTTP/1.1\r\nHost: \(authority)\r\nUser-Agent: AxonLinkPreview/1.0\r\nAccept: text/html,application/xhtml+xml,image/*;q=0.8\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        .utf8)
    let reader = Reader(
      connection: connection, url: url, kind: kind, request: request, cancellation: cancellation)
    reader.start(timeout: timeout)
    defer { reader.cancel() }
    return try reader.result.wait(timeout: timeout, cancellation: cancellation)
  }

  static func verify(_ trust: SecTrust, host: String) -> Bool {
    guard SecTrustSetPolicies(trust, SecPolicyCreateSSL(true, host as CFString)) == errSecSuccess,
      // Do not let an attacker-controlled AIA/OCSP URL create unchecked requests.
      SecTrustSetNetworkFetchAllowed(trust, false) == errSecSuccess
    else { return false }
    return SecTrustEvaluateWithError(trust, nil)
  }

  private final class Reader {
    let result = ShareIntakeResult<Response>()
    private let queue = DispatchQueue(label: "org.matrixaxon.axon.preview-http")
    private let connection: NWConnection
    private let url: URL
    private let request: Data
    private let cancellation: ShareCancellation
    private var parser: PreviewHTTPParser
    private var done = false
    private var timer: DispatchWorkItem?
    init(
      connection: NWConnection, url: URL, kind: Kind, request: Data, cancellation: ShareCancellation
    ) {
      self.connection = connection
      self.url = url
      self.request = request
      self.cancellation = cancellation
      parser = PreviewHTTPParser(kind: kind)
    }
    func start(timeout: TimeInterval) {
      queue.async {
        do { try self.cancellation.check() } catch {
          self.finish(.failure(error))
          return
        }
        self.connection.stateUpdateHandler = { state in
          guard !self.done else { return }
          do { try self.cancellation.check() } catch {
            self.finish(.failure(error))
            return
          }
          switch state {
          case .ready:
            self.connection.send(
              content: self.request,
              completion: .contentProcessed { error in
                if error != nil {
                  self.finish(.failure(ShareFailure.unavailable))
                } else {
                  self.receive()
                }
              })
          case .failed: self.finish(.failure(ShareFailure.unavailable))
          default: break
          }
        }
        let timer = DispatchWorkItem { [weak self] in self?.finish(.failure(ShareFailure.expired)) }
        self.timer = timer
        self.queue.asyncAfter(deadline: .now() + timeout, execute: timer)
        self.connection.start(queue: self.queue)
      }
    }
    func cancel() { queue.async { self.finish(.failure(ShareFailure.cancelled)) } }
    private func finish(_ result: Result<Response, Error>) {
      guard !done else { return }
      done = true
      timer?.cancel()
      timer = nil
      connection.stateUpdateHandler = nil
      connection.cancel()
      self.result.finish(result)
    }
    private func receive() {
      guard !done else { return }
      connection.receive(minimumIncompleteLength: 1, maximumLength: 16 * 1024) {
        data, _, complete, error in
        guard !self.done else { return }
        do {
          if try self.parser.append(data ?? Data(), eof: complete) {
            self.finish(
              .success(
                Response(
                  status: self.parser.status, headers: self.parser.headers, body: self.parser.body,
                  url: self.url)))
          } else if error != nil || complete {
            self.finish(.failure(ShareFailure.invalidResponse))
          } else {
            self.receive()
          }
        } catch { self.finish(.failure(error)) }
      }
    }
  }
}

/// Shared cancellable completion primitive, also usable by macOS transport tests.
final class ShareIntakeResult<T> {
  private let lock = NSLock()
  private let ready = DispatchSemaphore(value: 0)
  private(set) var value: Result<T, Error>?
  private var finished = false
  func finish(_ value: Result<T, Error>) {
    lock.lock()
    defer { lock.unlock() }
    guard !finished else { return }
    self.value = value
    finished = true
    ready.signal()
  }
  func wait(timeout: TimeInterval = 10, cancellation: ShareCancellation? = nil) throws -> T {
    defer {
      lock.lock()
      finished = true
      lock.unlock()
    }
    let deadline = DispatchTime.now() + max(0, timeout)
    while true {
      try cancellation?.check()
      if ready.wait(timeout: min(deadline, .now() + 0.1)) == .success {
        lock.lock()
        let result = value!
        lock.unlock()
        return try result.get()
      }
      guard DispatchTime.now() < deadline else { throw ShareFailure.expired }
    }
  }
}
