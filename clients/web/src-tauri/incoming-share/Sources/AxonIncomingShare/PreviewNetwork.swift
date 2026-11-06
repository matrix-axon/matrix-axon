import CFNetwork
import Darwin
import Foundation
import Network
import Security
import dnssd
import os

/// Dedicated, credential-free preview transport. DNS is cancellable; the TLS
/// connection receives only a validated numeric endpoint, never the hostname.
/// Hostname authentication still uses the original name. No URLSession, proxy,
/// cookies, certificate network fetches, script engine or automatic redirects.
enum PreviewNetwork {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "device-preview")
  struct Response {
    let status: Int
    let headers: [String: String]
    let body: Data
    let url: URL
  }
  enum Kind: String { case page, image }
  static let headBytes = 256 * 1024
  static let headerBytes = 32 * 1024

  struct Address: Equatable {
    let text: String
    let bytes: [UInt8]
    private let sourceIPv4: [UInt8]?
    private init(text: String, bytes: [UInt8], sourceIPv4: [UInt8]? = nil) {
      self.text = text
      self.bytes = bytes
      self.sourceIPv4 = sourceIPv4
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
    static func translated(_ ipv4: Address, using prefix: NAT64Prefix) -> Address? {
      guard ipv4.bytes.count == 4, ipv4.isPublic,
        let bytes = prefix.embedding(ipv4.bytes)
      else { return nil }
      var address = in6_addr()
      withUnsafeMutableBytes(of: &address) { $0.copyBytes(from: bytes) }
      var buffer = [CChar](repeating: 0, count: Int(INET6_ADDRSTRLEN))
      guard inet_ntop(AF_INET6, &address, &buffer, socklen_t(buffer.count)) != nil else { return nil }
      let candidate = Address(text: String(cString: buffer), bytes: bytes, sourceIPv4: ipv4.bytes)
      return candidate.isPublic ? candidate : nil
    }
    var approvedForConnection: Bool {
      if bytes.count == 4 { return isPublic }
      // A global IPv6 bit pattern may be an unknown operator NAT64 mapping to
      // a private IPv4 service. Only locally synthesized public-A targets pass.
      guard let sourceIPv4 else { return false }
      return Address(text: "", bytes: sourceIPv4).isPublic && isPublic
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

  typealias Resolve = (String, TimeInterval, ShareCancellation) throws -> [Address]
  typealias Exchange = (URL, String, [Address], Kind, DispatchTime, ShareCancellation) throws ->
    Response

  static func fetch(
    _ initial: URL, kind: Kind, deadline: DispatchTime, cancellation: ShareCancellation,
    resolve: Resolve = resolve, exchange: Exchange = exchange
  ) throws -> Response {
    log.info("device_preview_fetch_start kind=\(kind.rawValue, privacy: .public)")
    var url = initial
    for hop in 0..<6 {
      try cancellation.check()
      let host = try validate(url)
      let addresses: [Address]
      log.info("device_preview_dns_start kind=\(kind.rawValue, privacy: .public) hop=\(hop)")
      do { addresses = try resolve(host, min(4, remaining(deadline)), cancellation) } catch {
        log.info(
          "device_preview_dns_failed category=\((error as? ShareFailure)?.rawValue ?? "network", privacy: .public)"
        )
        throw error
      }
      guard (1...2).contains(addresses.count), addresses.allSatisfy(\.approvedForConnection) else {
        throw ShareFailure.previewUnavailable
      }
      try cancellation.check()
      _ = try remaining(deadline)
      let response = try exchange(url, host, addresses, kind, deadline, cancellation)
      log.info(
        "device_preview_response kind=\(kind.rawValue, privacy: .public) hop=\(hop) status=\(response.status) bytes=\(response.body.count)"
      )
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
    let result = ShareIntakeResult<Address?>()
    let family: DNSServiceProtocol
    let host: String
    let discovery: Bool
    var reference: DNSServiceRef?
    init(_ family: DNSServiceProtocol, host: String, discovery: Bool = false) {
      self.family = family
      self.host = host
      self.discovery = discovery
    }
  }
  // One worker per active lookup; deallocation is serialized with callbacks.
  static func resolve(_ host: String, timeout: TimeInterval, cancellation: ShareCancellation) throws
    -> [Address]
  {
    let queue = DispatchQueue(label: "org.matrixaxon.axon.preview-dns")
    // Never query an origin's AAAA records. A separate fixed discovery query
    // can synthesize NAT64 connectivity only from its validated public A record.
    // Both cancellable queries share the original bounded DNS budget.
    let queries = [
      DNSResult(DNSServiceProtocol(kDNSServiceProtocol_IPv4), host: host),
      DNSResult(DNSServiceProtocol(kDNSServiceProtocol_IPv6), host: "ipv4only.arpa.", discovery: true),
    ]
    let deadline = DispatchTime.now() + timeout
    queue.async {
      for state in queries {
        do { try cancellation.check() } catch {
          state.result.finish(.failure(error))
          return
        }
        guard DispatchTime.now() < deadline else {
          state.result.finish(.failure(ShareFailure.expired))
          return
        }
        // Surface authoritative negative answers instead of waiting out the
        // budget: no origin A is a policy failure, and no DNS64 is optional.
        let status = DNSServiceGetAddrInfo(
          &state.reference, DNSServiceFlags(kDNSServiceFlagsReturnIntermediates), 0,
          state.family, state.host,
          { _, flags, _, error, _, address, _, context in
            guard let context else { return }
            let state = Unmanaged<DNSResult>.fromOpaque(context).takeUnretainedValue()
            if error == kDNSServiceErr_NoSuchRecord {
              state.result.finish(.success(nil))
              return
            }
            guard error == 0, flags & kDNSServiceFlagsAdd != 0, let address else {
              state.result.finish(.failure(ShareFailure.unavailable))
              return
            }
            var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard
              getnameinfo(
                address, socklen_t(address.pointee.sa_len), &buffer, socklen_t(buffer.count), nil,
                0,
                NI_NUMERICHOST) == 0,
              let parsed = Address.parse(String(cString: buffer)),
              (state.discovery ? parsed.bytes.count == 16 : parsed.bytes.count == 4 && parsed.isPublic)
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
    }
    defer {
      queue.async {
        for state in queries {
          if let reference = state.reference {
            DNSServiceRefDeallocate(reference)
            state.reference = nil
          }
        }
      }
    }
    return try resolvedAddresses(
      queries.map(\.result), deadline: deadline, cancellation: cancellation)
  }

  static func resolvedAddresses(
    _ queries: [ShareIntakeResult<Address?>], deadline: DispatchTime,
    cancellation: ShareCancellation
  ) throws -> [Address] {
    guard queries.count == 2 else { throw ShareFailure.previewUnavailable }
    try cancellation.check()
    guard let ipv4 = try queries[0].wait(until: deadline, cancellation: cancellation) else {
      throw ShareFailure.publicIPv4Required
    }
    guard ipv4.bytes.count == 4, ipv4.isPublic else { throw ShareFailure.previewUnavailable }
    var addresses = [ipv4]
    do {
      if let probe = try queries[1].wait(until: deadline, cancellation: cancellation),
        let prefix = NAT64Prefix.discover(probe),
        let translated = Address.translated(ipv4, using: prefix)
      {
        addresses.insert(translated, at: 0)
      }
    } catch ShareFailure.cancelled {
      throw ShareFailure.cancelled
    } catch {
      // Optional discovery cannot invalidate a usable public IPv4 target.
    }
    try cancellation.check()
    log.info(
      "device_preview_dns_complete ipv6=\(addresses.contains { $0.bytes.count == 16 }) ipv4=\(addresses.contains { $0.bytes.count == 4 })"
    )
    return addresses
  }

  static func exchange(
    _ url: URL, host: String, addresses: [Address], kind: Kind,
    deadline: DispatchTime, cancellation: ShareCancellation
  ) throws -> Response {
    try cancellation.check()
    _ = try remaining(deadline)
    guard try validate(url) == host, (1...2).contains(addresses.count),
      addresses.allSatisfy(\.approvedForConnection),
      let components = URLComponents(url: url, resolvingAgainstBaseURL: false)
    else { throw ShareFailure.previewUnavailable }
    let queue = DispatchQueue(label: "org.matrixaxon.axon.preview-http")
    let port = NWEndpoint.Port(rawValue: UInt16(components.port ?? 443))!
    let connections = addresses.map { address in
      (address: address, connection: makeConnection(host: host, address: address, port: port))
    }
    let race = PreviewConnectionRace(connections, queue: queue) { candidate, queue, complete in
      let family = candidate.address.bytes.count == 4 ? "ipv4" : "ipv6"
      log.info("device_preview_connect_start family=\(family, privacy: .public)")
      candidate.connection.stateUpdateHandler = { state in
        switch state {
        case .ready:
          log.info("device_preview_connect_ready family=\(family, privacy: .public)")
          complete(.success(()))
        case .failed(let error):
          log.info(
            "device_preview_connect_failed family=\(family, privacy: .public) code=\(networkCode(error))"
          )
          complete(.failure(ShareFailure.unavailable))
        case .waiting(let error):
          log.info(
            "device_preview_connect_waiting family=\(family, privacy: .public) code=\(networkCode(error))"
          )
        default: break
        }
      }
      candidate.connection.start(queue: queue)
    } stop: { candidate in
      candidate.connection.stateUpdateHandler = nil
      candidate.connection.cancel()
    }
    let connection: NWConnection
    do {
      let winner = try race.connect(deadline: deadline, cancellation: cancellation)
      connection = winner.connection
      let family = winner.address.bytes.count == 4 ? "ipv4" : "ipv6"
      log.info("device_preview_connect_winner family=\(family, privacy: .public)")
    } catch {
      log.info(
        "device_preview_connect_ended category=\((error as? ShareFailure)?.rawValue ?? "network", privacy: .public)"
      )
      throw error
    }
    queue.sync { connection.stateUpdateHandler = nil }
    let target =
      (components.percentEncodedPath.isEmpty ? "/" : components.percentEncodedPath)
      + (components.percentEncodedQuery.map { "?" + $0 } ?? "")
    let authority = host + (components.port.map { ":\($0)" } ?? "")
    let request = Data(
      "GET \(target) HTTP/1.1\r\nHost: \(authority)\r\nUser-Agent: AxonLinkPreview/1.0\r\nAccept: text/html,application/xhtml+xml,image/*;q=0.8\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        .utf8)
    let reader = Reader(
      connection: connection, queue: queue, url: url, kind: kind, request: request,
      cancellation: cancellation)
    reader.start(deadline: deadline)
    defer { reader.cancel() }
    return try reader.result.wait(until: deadline, cancellation: cancellation)
  }

  private static func networkCode(_ error: NWError) -> Int32 {
    switch error {
    case .posix(let value): return value.rawValue
    case .dns(let value), .tls(let value): return value
    default: return 0
    }
  }

  private static func makeConnection(host: String, address: Address, port: NWEndpoint.Port)
    -> NWConnection
  {
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
    return NWConnection(host: address.endpoint, port: port, using: parameters)
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
    private let queue: DispatchQueue
    private let connection: NWConnection
    private let url: URL
    private let request: Data
    private let cancellation: ShareCancellation
    private var parser: PreviewHTTPParser
    private var done = false
    private var timer: DispatchWorkItem?
    init(
      connection: NWConnection, queue: DispatchQueue, url: URL, kind: Kind, request: Data,
      cancellation: ShareCancellation
    ) {
      self.connection = connection
      self.queue = queue
      self.url = url
      self.request = request
      self.cancellation = cancellation
      parser = PreviewHTTPParser(kind: kind)
    }
    func start(deadline: DispatchTime) {
      queue.async {
        do {
          try self.cancellation.check()
          _ = try remaining(deadline)
        } catch {
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
          case .failed: self.finish(.failure(ShareFailure.unavailable))
          default: break
          }
        }
        let timer = DispatchWorkItem { [weak self] in
          guard let self else { return }
          log.info("device_preview_http_timeout headers_received=\(self.parser.status != 0)")
          self.finish(.failure(ShareFailure.expired))
        }
        self.timer = timer
        self.queue.asyncAfter(deadline: deadline, execute: timer)
        self.connection.send(
          content: self.request,
          completion: .contentProcessed { error in
            if error != nil {
              self.finish(.failure(ShareFailure.unavailable))
            } else {
              self.receive()
            }
          })
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
    try wait(until: .now() + max(0, timeout), cancellation: cancellation)
  }
  func wait(until deadline: DispatchTime, cancellation: ShareCancellation? = nil) throws -> T {
    defer {
      lock.lock()
      finished = true
      lock.unlock()
    }
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
