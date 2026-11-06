import Foundation
import XCTest

@testable import AxonIncomingShare

final class PreviewNAT64Tests: XCTestCase {
  func testAllRFC6052LayoutsTranslateOnlyTheApprovedPublicIPv4() throws {
    // Independent wire vectors: prefix length, RFC 7050 probe, translated 8.8.4.4.
    let vectors: [(Int, String, String)] = [
      (32, "2606:4700:c000:aa::", "2606:4700:808:404::"),
      (40, "2606:4700:12c0:0:aa::", "2606:4700:1208:804:4::"),
      (48, "2606:4700:1234:c000:0:aa00::", "2606:4700:1234:808:4:400::"),
      (56, "2606:4700:1234:56c0:0:aa::", "2606:4700:1234:5608:8:404::"),
      (64, "2606:4700:1234:5678:c0:0:aa00:0", "2606:4700:1234:5678:8:804:400:0"),
      (96, "2606:4700:1234:5678:0:abcd:c000:aa", "2606:4700:1234:5678:0:abcd:808:404"),
      (96, "64:ff9b::c000:aa", "64:ff9b::808:404"),
      (96, "64:ff9b::c000:ab", "64:ff9b::808:404"),
    ]
    for (length, probe, expected) in vectors {
      let prefix = try XCTUnwrap(
        PreviewNetwork.NAT64Prefix.discover(PreviewNetwork.Address.parse(probe)!))
      XCTAssertEqual(prefix.length, length, probe)
      let target = try XCTUnwrap(
        PreviewNetwork.Address.translated(PreviewNetwork.Address.parse("8.8.4.4")!, using: prefix))
      XCTAssertEqual(target.bytes, PreviewNetwork.Address.parse(expected)?.bytes, probe)
      XCTAssertTrue(target.approvedForConnection)
      // DNS-supplied copies of the same bits do not carry public-A provenance.
      XCTAssertFalse(PreviewNetwork.Address.parse(expected)!.approvedForConnection)
      for unsafe in ["10.0.0.1", "127.0.0.1", "169.254.169.254", "192.168.1.1", "::1"] {
        XCTAssertNil(
          PreviewNetwork.Address.translated(PreviewNetwork.Address.parse(unsafe)!, using: prefix))
      }
    }
  }
  func testMalformedDiscoveryCannotAuthorizeATranslation() throws {
    for probe in [
      "1.1.1.1", "::1", "fc00::c000:aa", "2001:db8::c000:aa",
      "2606:4700::1111", "2606:4700:1234:5678:100::c000:aa",
      "2606:4700:c000:aa::1",
    ] {
      XCTAssertNil(PreviewNetwork.NAT64Prefix.discover(PreviewNetwork.Address.parse(probe)!), probe)
    }
  }
}
