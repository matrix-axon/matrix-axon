import Foundation

extension PreviewNetwork {
  /// RFC 7050 is used only to translate an already-vetted public IPv4 target.
  /// It never authorizes an origin's AAAA records or guesses their provenance.
  struct NAT64Prefix: Equatable {
    let length: Int
    private let bytes: [UInt8]
    private init(length: Int, bytes: [UInt8]) {
      self.length = length
      self.bytes = bytes
    }

    static func discover(_ probe: Address) -> NAT64Prefix? {
      guard probe.bytes.count == 16, probe.bytes[8] == 0 else { return nil }
      var matches: [NAT64Prefix] = []
      let sentinels: [[UInt8]] = [[192, 0, 0, 170], [192, 0, 0, 171]]
      for length in [32, 40, 48, 56, 64, 96] {
        var bytes = [UInt8](repeating: 0, count: 16)
        bytes.replaceSubrange(0..<(length / 8), with: probe.bytes.prefix(length / 8))
        let prefix = NAT64Prefix(length: length, bytes: bytes)
        // Require the complete canonical encoding, including the reserved u
        // octet and zero suffix; ignore malformed or ambiguous discovery data.
        if sentinels.contains(where: {
          prefix.embedding($0) == probe.bytes
        }), Address.translated(Address.parse("1.1.1.1")!, using: prefix) != nil {
          matches.append(prefix)
        }
      }
      return matches.count == 1 ? matches[0] : nil
    }

    func embedding(_ ipv4: [UInt8]) -> [UInt8]? {
      guard ipv4.count == 4 else { return nil }
      var result = bytes
      var offset = length / 8
      for byte in ipv4 {
        if offset == 8 { offset += 1 }
        result[offset] = byte
        offset += 1
      }
      return result
    }
  }
}
