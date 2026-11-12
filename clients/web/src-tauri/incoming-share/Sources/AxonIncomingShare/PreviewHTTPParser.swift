import CFNetwork
import Foundation

/// CFNetwork parses headers; this bounded collector handles HTTP/1.1 body
/// framing. HTML intentionally stops after its head or byte budget. Images
/// require a complete body. No received bytes are forwarded to another server.
struct PreviewHTTPParser {
  private(set) var status = 0
  private(set) var headers: [String: String] = [:]
  private(set) var body = Data()
  private var pending = Data()
  private let kind: PreviewNetwork.Kind
  private var length: Int?
  private var chunked = false
  private var chunk: Int?
  private var chunks = 0
  private var endingChunks = false
  private var interim = 0
  private var finished = false
  private var prefix: Bool {
    kind == .page && !(headers["content-type"] ?? "").lowercased().hasPrefix("image/")
  }
  private var limit: Int { prefix ? PreviewNetwork.headBytes : ShareLimits.thumbnailBytes }
  init(kind: PreviewNetwork.Kind) { self.kind = kind }

  mutating func append(_ data: Data, eof: Bool = false) throws -> Bool {
    guard !finished else { return true }
    guard data.count <= 16 * 1024 else { throw ShareFailure.quota }
    pending.append(data)
    while status == 0 {
      guard let end = pending.range(of: Data("\r\n\r\n".utf8)) else {
        guard pending.count <= PreviewNetwork.headerBytes, !eof else {
          throw ShareFailure.invalidResponse
        }
        return false
      }
      let count = pending.distance(from: pending.startIndex, to: end.upperBound)
      guard count <= PreviewNetwork.headerBytes else { throw ShareFailure.quota }
      let header = Data(pending.prefix(count))
      pending.removeFirst(count)
      let message = CFHTTPMessageCreateEmpty(kCFAllocatorDefault, false).takeRetainedValue()
      guard
        header.withUnsafeBytes({
          CFHTTPMessageAppendBytes(
            message, $0.bindMemory(to: UInt8.self).baseAddress!, header.count)
        }),
        CFHTTPMessageIsHeaderComplete(message),
        let fields = CFHTTPMessageCopyAllHeaderFields(message)?.takeRetainedValue()
          as? [String: String]
      else { throw ShareFailure.invalidResponse }
      let code = CFHTTPMessageGetResponseStatusCode(message)
      guard (100...599).contains(code), code != 101 else { throw ShareFailure.invalidResponse }
      if code < 200 {
        interim += 1
        guard interim <= 5 else { throw ShareFailure.invalidResponse }
        continue
      }
      status = code
      headers = fields.reduce(into: [String: String]()) {
        $0[$1.key.lowercased()] = $1.value.trimmingCharacters(in: CharacterSet.whitespaces)
      }
      // Reject ambiguous framing, folded headers and duplicate framing fields.
      guard let raw = String(data: header, encoding: .isoLatin1) else {
        throw ShareFailure.invalidResponse
      }
      let lines = raw.components(separatedBy: "\r\n").dropFirst().filter { !$0.isEmpty }
      guard lines.allSatisfy({ !$0.hasPrefix(" ") && !$0.hasPrefix("\t") }),
        lines.filter({ $0.lowercased().hasPrefix("content-length:") }).count <= 1,
        lines.filter({ $0.lowercased().hasPrefix("transfer-encoding:") }).count <= 1
      else { throw ShareFailure.invalidResponse }
      if [301, 302, 303, 307, 308].contains(code) {
        finished = true
        return true
      }
      guard code == 200 else { throw ShareFailure.previewUnavailable }
      guard headers["content-encoding"].map({ $0.lowercased() == "identity" }) ?? true else {
        throw ShareFailure.invalidResponse
      }
      if let transfer = headers["transfer-encoding"] {
        guard transfer.lowercased() == "chunked", headers["content-length"] == nil else {
          throw ShareFailure.invalidResponse
        }
        chunked = true
      } else if let rawLength = headers["content-length"] {
        guard !rawLength.isEmpty, rawLength.allSatisfy({ $0.isASCII && $0.isNumber }),
          let value = Int(rawLength), value >= 0
        else { throw ShareFailure.invalidResponse }
        if !prefix && value > limit { throw ShareFailure.quota }
        length = value
      }
    }
    if chunked {
      while true {
        if endingChunks {
          if pending.starts(with: Data("\r\n".utf8)) {
            finished = true
            return true
          }
          if let end = pending.range(of: Data("\r\n\r\n".utf8)) {
            guard
              pending.distance(from: pending.startIndex, to: end.upperBound)
                <= PreviewNetwork.headerBytes
            else { throw ShareFailure.quota }
            finished = true
            return true
          }
          guard pending.count <= PreviewNetwork.headerBytes, !eof else {
            throw ShareFailure.invalidResponse
          }
          return false
        }
        if chunk == nil {
          guard let end = pending.range(of: Data("\r\n".utf8)) else {
            guard pending.count <= 1024, !eof else { throw ShareFailure.invalidResponse }
            return false
          }
          let bytes = pending.distance(from: pending.startIndex, to: end.lowerBound)
          guard bytes <= 1024,
            let line = String(data: pending.prefix(bytes), encoding: .ascii),
            let sizeText = line.split(
              separator: ";", maxSplits: 1, omittingEmptySubsequences: false
            ).first,
            !sizeText.isEmpty, sizeText.count <= 16,
            sizeText.allSatisfy({ $0.isHexDigit }), let size = Int(sizeText, radix: 16)
          else { throw ShareFailure.invalidResponse }
          pending.removeFirst(bytes + 2)
          chunks += 1
          guard chunks <= 4096 else { throw ShareFailure.quota }
          if size == 0 {
            endingChunks = true
            continue
          }
          if !prefix && size > limit - body.count { throw ShareFailure.quota }
          chunk = size
        }
        let wanted = min(chunk!, pending.count)
        if wanted > 0 {
          let consumed = try collect(pending.prefix(wanted))
          pending.removeFirst(consumed)
          if finished { return true }
          chunk! -= consumed
        }
        if chunk! > 0 {
          guard !eof else { throw ShareFailure.invalidResponse }
          return false
        }
        guard pending.count >= 2 else {
          guard !eof else { throw ShareFailure.invalidResponse }
          return false
        }
        guard pending.prefix(2) == Data("\r\n".utf8) else { throw ShareFailure.invalidResponse }
        pending.removeFirst(2)
        chunk = nil
      }
    }
    let wanted = min(pending.count, length.map { max(0, $0 - body.count) } ?? pending.count)
    if wanted > 0 {
      let consumed = try collect(pending.prefix(wanted))
      pending.removeFirst(consumed)
    }
    if finished { return true }
    if let length, body.count == length {
      finished = true
      return true
    }
    if eof {
      guard length == nil else { throw ShareFailure.invalidResponse }
      finished = true
      return true
    }
    return false
  }

  private mutating func collect(_ data: Data.SubSequence) throws -> Int {
    let wanted = min(data.count, limit - body.count)
    if !prefix && wanted != data.count { throw ShareFailure.quota }
    body.append(data.prefix(wanted))
    if prefix {
      // Only scan the bounded new tail; do not rescan a large head per packet.
      let tail = body.suffix(wanted + 8)
      if Self.endsHead(tail) || body.count == limit {
        finished = true
      }
    }
    return wanted
  }

  /// `</head` or `<body`, in any case, followed by what ends an HTML tag name:
  /// whitespace, `/` or `>`. So `<bodyguard>` does not end a head. One inside
  /// script or comment text still does; the reader does not tokenize. The
  /// Rust reader (`axon-link-preview/src/http.rs`) applies the same rule.
  static func endsHead(_ bytes: Data) -> Bool {
    let bytes = [UInt8](bytes)
    let boundary: Set<UInt8> = [9, 10, 12, 13, 32, 47, 62]
    for marker in [[UInt8]("</head".utf8), [UInt8]("<body".utf8)] where bytes.count > marker.count {
      for start in 0..<(bytes.count - marker.count) {
        var matches = true
        for offset in 0..<marker.count {
          let byte = bytes[start + offset]
          let lower = (65...90).contains(byte) ? byte + 32 : byte
          if lower != marker[offset] {
            matches = false
            break
          }
        }
        if matches && boundary.contains(bytes[start + marker.count]) { return true }
      }
    }
    return false
  }
}
